use crate::api::errors::FlagError;
use crate::api::types::{FlagDetails, FlagsResponse, FromFeatureAndMatch};
use crate::client::database::Client as DatabaseClient;
use crate::cohort::cohort_cache_manager::CohortCacheManager;
use crate::cohort::cohort_models::{Cohort, CohortId};
use crate::flags::flag_match_reason::FeatureFlagMatchReason;
use crate::flags::flag_models::{FeatureFlag, FeatureFlagList, FlagGroupType};
use crate::metrics::consts::{
    DB_GROUP_PROPERTIES_READS_COUNTER, DB_PERSON_AND_GROUP_PROPERTIES_READS_COUNTER,
    DB_PERSON_PROPERTIES_READS_COUNTER, FLAG_DB_PROPERTIES_FETCH_TIME,
    FLAG_EVALUATE_ALL_CONDITIONS_TIME, FLAG_EVALUATION_ERROR_COUNTER, FLAG_EVALUATION_TIME,
    FLAG_GET_MATCH_TIME, FLAG_GROUP_FETCH_TIME, FLAG_HASH_KEY_PROCESSING_TIME,
    FLAG_HASH_KEY_WRITES_COUNTER, FLAG_LOCAL_PROPERTY_OVERRIDE_MATCH_TIME,
    FLAG_STATIC_COHORT_DB_EVALUATION_TIME, PROPERTY_CACHE_HITS_COUNTER,
    PROPERTY_CACHE_MISSES_COUNTER,
};
use crate::metrics::utils::parse_exception_for_prometheus_label;
use crate::properties::property_matching::match_property;
use crate::properties::property_models::{OperatorType, PropertyFilter};
use anyhow::Result;
use common_metrics::inc;
use common_types::{ProjectId, TeamId};
use petgraph::algo::{is_cyclic_directed, toposort};
use petgraph::graph::DiGraph;
use serde_json::Value;
use sha1::{Digest, Sha1};
use sqlx::{postgres::PgQueryResult, Acquire, FromRow, Row};
use std::collections::hash_map::Entry;
use std::fmt::Write;
use std::sync::Arc;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::Duration,
};
use tokio::time::{sleep, timeout};
use tracing::{error, info};

#[cfg(test)]
use crate::api::types::{FlagValue, LegacyFlagsResponse}; // Only used in the tests

pub type PersonId = i64;
pub type GroupTypeIndex = i32;
pub type PostgresReader = Arc<dyn DatabaseClient + Send + Sync>;
pub type PostgresWriter = Arc<dyn DatabaseClient + Send + Sync>;

#[derive(Debug)]
struct SuperConditionEvaluation {
    should_evaluate: bool,
    is_match: bool,
    reason: FeatureFlagMatchReason,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct FeatureFlagMatch {
    pub matches: bool,
    pub variant: Option<String>,
    pub reason: FeatureFlagMatchReason,
    pub condition_index: Option<usize>,
    pub payload: Option<Value>,
}

#[derive(Debug, FromRow)]
pub struct GroupTypeMapping {
    pub group_type: String,
    pub group_type_index: GroupTypeIndex,
}

/// This struct is a cache for group type mappings, which are stored in a DB.  We use these mappings
/// to look up group names based on the group aggregation indices stored on flag filters, which lets us
/// perform group property matching.  We cache them per request so that we can perform multiple flag evaluations
/// without needing to fetch the mappings from the DB each time.
/// Typically, the mappings look like this:
///
/// let group_types = vec![
///     ("project", 0),
///     ("organization", 1),
///     ("instance", 2),
///     ("customer", 3),
///     ("team", 4),  ];
///
/// But for backwards compatibility, we also support whatever mappings may lie in the table.
/// These mappings are ingested via the plugin server.
#[derive(Clone)]
pub struct GroupTypeMappingCache {
    project_id: ProjectId,
    failed_to_fetch_flags: bool,
    group_types_to_indexes: HashMap<String, GroupTypeIndex>,
    group_indexes_to_types: HashMap<GroupTypeIndex, String>,
    reader: PostgresReader,
}

impl GroupTypeMappingCache {
    pub fn new(project_id: ProjectId, reader: PostgresReader) -> Self {
        GroupTypeMappingCache {
            project_id,
            failed_to_fetch_flags: false,
            group_types_to_indexes: HashMap::new(),
            group_indexes_to_types: HashMap::new(),
            reader,
        }
    }

    pub async fn group_type_to_group_type_index_map(
        &mut self,
    ) -> Result<HashMap<String, GroupTypeIndex>, FlagError> {
        if self.failed_to_fetch_flags {
            return Err(FlagError::DatabaseUnavailable);
        }

        if !self.group_types_to_indexes.is_empty() {
            return Ok(self.group_types_to_indexes.clone());
        }

        let mapping = match self
            .fetch_group_type_mapping(self.reader.clone(), self.project_id)
            .await
        {
            Ok(mapping) if !mapping.is_empty() => mapping,
            Ok(_) => {
                self.failed_to_fetch_flags = true;
                let reason = "no_group_type_mappings";
                inc(
                    FLAG_EVALUATION_ERROR_COUNTER,
                    &[("reason".to_string(), reason.to_string())],
                    1,
                );
                return Err(FlagError::NoGroupTypeMappings);
            }
            Err(e) => {
                self.failed_to_fetch_flags = true;
                let reason = parse_exception_for_prometheus_label(&e);
                inc(
                    FLAG_EVALUATION_ERROR_COUNTER,
                    &[("reason".to_string(), reason.to_string())],
                    1,
                );
                return Err(e);
            }
        };
        self.group_types_to_indexes.clone_from(&mapping);

        Ok(mapping)
    }

    pub async fn group_type_index_to_group_type_map(
        &mut self,
    ) -> Result<HashMap<GroupTypeIndex, String>, FlagError> {
        if !self.group_indexes_to_types.is_empty() {
            return Ok(self.group_indexes_to_types.clone());
        }

        let types_to_indexes = self.group_type_to_group_type_index_map().await?;
        let result: HashMap<GroupTypeIndex, String> =
            types_to_indexes.into_iter().map(|(k, v)| (v, k)).collect();

        if !result.is_empty() {
            self.group_indexes_to_types.clone_from(&result);
            Ok(result)
        } else {
            let reason = "no_group_type_mappings";
            inc(
                FLAG_EVALUATION_ERROR_COUNTER,
                &[("reason".to_string(), reason.to_string())],
                1,
            );
            Err(FlagError::NoGroupTypeMappings)
        }
    }

    async fn fetch_group_type_mapping(
        &mut self,
        reader: PostgresReader,
        project_id: ProjectId,
    ) -> Result<HashMap<String, GroupTypeIndex>, FlagError> {
        let mut conn = reader.as_ref().get_connection().await?;

        let query = r#"
            SELECT group_type, group_type_index 
            FROM posthog_grouptypemapping 
            WHERE project_id = $1
        "#;

        let rows = sqlx::query_as::<_, GroupTypeMapping>(query)
            .bind(project_id)
            .fetch_all(&mut *conn)
            .await?;

        let mapping: HashMap<String, GroupTypeIndex> = rows
            .into_iter()
            .map(|row| (row.group_type, row.group_type_index))
            .collect();

        if mapping.is_empty() {
            let reason = "no_group_type_mappings";
            inc(
                FLAG_EVALUATION_ERROR_COUNTER,
                &[("reason".to_string(), reason.to_string())],
                1,
            );
            Err(FlagError::NoGroupTypeMappings)
        } else {
            Ok(mapping)
        }
    }
}

/// This struct maintains evaluation state by caching database-sourced data during feature flag evaluation.
/// It stores person IDs, properties, group properties, and cohort matches that are fetched from the database,
/// allowing them to be reused across multiple flag evaluations within the same request without additional DB lookups.
///
/// The cache is scoped to a single evaluation session and is cleared between different requests.
#[derive(Clone, Default, Debug)]
pub struct FlagEvaluationState {
    /// The person ID associated with the distinct_id being evaluated
    person_id: Option<PersonId>,
    /// Properties associated with the person, fetched from the database
    person_properties: Option<HashMap<String, Value>>,
    /// Properties for each group type involved in flag evaluation
    group_properties: HashMap<GroupTypeIndex, HashMap<String, Value>>,
    /// Cache of static cohort membership results to avoid repeated DB lookups
    static_cohort_matches: Option<HashMap<CohortId, bool>>,
}

/// Represents the group-related data needed for feature flag evaluation
#[derive(Debug)]
struct GroupEvaluationData {
    /// Set of group type indexes required for flag evaluation
    type_indexes: HashSet<GroupTypeIndex>,
    /// Set of group keys that need to be evaluated
    keys: HashSet<String>,
}

/// Evaluates feature flags for a specific user/group context.
///
/// This struct maintains the state and logic needed to evaluate feature flags, including:
/// - User identification (distinct_id, team_id)
/// - Database connections for fetching data
/// - Caches for properties, cohorts, and group mappings to optimize performance
/// - Evaluation state that persists across multiple flag evaluations in a request
///
/// The matcher is typically created once per request and can evaluate multiple flags
/// efficiently by reusing cached data and DB connections.
#[derive(Clone)]
pub struct FeatureFlagMatcher {
    /// Unique identifier for the user/entity being evaluated
    pub distinct_id: String,
    /// Team ID for scoping flag evaluations
    pub team_id: TeamId,
    /// Project ID for scoping flag evaluations
    pub project_id: ProjectId,
    /// Database connection for reading data
    pub reader: PostgresReader,
    /// Database connection for writing data (e.g. experience continuity overrides)
    pub writer: PostgresWriter,
    /// Cache manager for cohort definitions and memberships
    pub cohort_cache: Arc<CohortCacheManager>,
    /// Cache for mapping between group types and their indices
    group_type_mapping_cache: GroupTypeMappingCache,
    /// State maintained during flag evaluation, including cached DB lookups
    flag_evaluation_state: FlagEvaluationState,
    /// Group key mappings for group-based flag evaluation
    groups: HashMap<String, Value>,
}

const LONG_SCALE: u64 = 0xfffffffffffffff;

impl FeatureFlagMatcher {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        distinct_id: String,
        team_id: TeamId,
        project_id: ProjectId,
        reader: PostgresReader,
        writer: PostgresWriter,
        cohort_cache: Arc<CohortCacheManager>,
        group_type_mapping_cache: Option<GroupTypeMappingCache>,
        groups: Option<HashMap<String, Value>>,
    ) -> Self {
        FeatureFlagMatcher {
            distinct_id,
            team_id,
            project_id,
            reader: reader.clone(),
            writer: writer.clone(),
            cohort_cache,
            group_type_mapping_cache: group_type_mapping_cache
                .unwrap_or_else(|| GroupTypeMappingCache::new(project_id, reader.clone())),
            groups: groups.unwrap_or_default(),
            flag_evaluation_state: FlagEvaluationState::default(),
        }
    }

    /// Evaluates all feature flags for the current matcher context.
    ///
    /// ## Arguments
    ///
    /// * `feature_flags` - The list of feature flags to evaluate.
    /// * `person_property_overrides` - Any overrides for person properties.
    /// * `group_property_overrides` - Any overrides for group properties.
    /// * `hash_key_override` - Optional hash key overrides for experience continuity.
    ///
    /// ## Returns
    ///
    /// * `FlagsResponse` - The result containing flag evaluations and any errors.
    pub async fn evaluate_all_feature_flags(
        &mut self,
        feature_flags: FeatureFlagList,
        person_property_overrides: Option<HashMap<String, Value>>,
        group_property_overrides: Option<HashMap<String, HashMap<String, Value>>>,
        hash_key_override: Option<String>,
    ) -> FlagsResponse {
        let eval_timer = common_metrics::timing_guard(FLAG_EVALUATION_TIME, &[]);

        let flags_have_experience_continuity_enabled = feature_flags
            .flags
            .iter()
            .any(|flag| flag.ensure_experience_continuity);

        // Process any hash key overrides
        let hash_key_timer = common_metrics::timing_guard(FLAG_HASH_KEY_PROCESSING_TIME, &[]);
        let (hash_key_overrides, initial_error) = if flags_have_experience_continuity_enabled {
            match hash_key_override {
                Some(hash_key) => {
                    let target_distinct_ids = vec![self.distinct_id.clone(), hash_key.clone()];
                    self.process_hash_key_override(hash_key, target_distinct_ids)
                        .await
                }
                // if a flag has experience continuity enabled but no hash key override is provided,
                // we don't need to write an override, we can just use the distinct_id
                None => (None, false),
            }
        } else {
            // if experience continuity is not enabled, we don't need to worry about hash key overrides
            (None, false)
        };
        hash_key_timer
            .label("outcome", if initial_error { "error" } else { "success" })
            .fin();

        // If there was an initial error in processing hash key overrides, increment the error counter
        if initial_error {
            let reason = "hash_key_override_error";
            common_metrics::inc(
                FLAG_EVALUATION_ERROR_COUNTER,
                &[("reason".to_string(), reason.to_string())],
                1,
            );
        }

        let flags_response = self
            .evaluate_flags_with_overrides(
                feature_flags,
                person_property_overrides,
                group_property_overrides,
                hash_key_overrides,
            )
            .await;

        eval_timer
            .label(
                "outcome",
                if flags_response.errors_while_computing_flags || initial_error {
                    "error"
                } else {
                    "success"
                },
            )
            .fin();

        FlagsResponse::new(
            initial_error || flags_response.errors_while_computing_flags,
            flags_response.flags,
            None,
        )
    }

    /// Processes hash key overrides for feature flags with experience continuity enabled.
    ///
    /// This method handles the logic for managing hash key overrides, which are used to ensure
    /// consistent feature flag experiences across different distinct IDs (e.g., when a user logs in).
    /// It performs the following steps:
    ///
    /// 1. Checks if a hash key override needs to be written by comparing the current distinct ID
    ///    with the provided hash key
    /// 2. If needed, writes the hash key override to the database using the writer connection
    /// 3. Increments metrics to track successful/failed hash key override writes
    /// 4. Retrieves and returns the current hash key overrides for the target distinct IDs
    ///
    /// Returns a tuple containing:
    /// - Option<HashMap<String, String>>: The hash key overrides if successfully retrieved, None if there was an error
    /// - bool: Whether there was an error during processing (true = error occurred)
    async fn process_hash_key_override(
        &self,
        hash_key: String,
        target_distinct_ids: Vec<String>,
    ) -> (Option<HashMap<String, String>>, bool) {
        let should_write = match should_write_hash_key_override(
            self.reader.clone(),
            self.team_id,
            self.distinct_id.clone(),
            self.project_id,
            hash_key.clone(),
        )
        .await
        {
            Ok(should_write) => should_write,
            Err(e) => {
                error!(
                    "Failed to check if hash key override should be written: {:?}",
                    e
                );
                let reason = parse_exception_for_prometheus_label(&e);
                inc(
                    FLAG_EVALUATION_ERROR_COUNTER,
                    &[("reason".to_string(), reason.to_string())],
                    1,
                );
                return (None, true);
            }
        };

        let mut writing_hash_key_override = false;

        if should_write {
            if let Err(e) = set_feature_flag_hash_key_overrides(
                // NB: this is the only method that writes to the database, so it's the only one that should use the writer
                self.writer.clone(),
                self.team_id,
                target_distinct_ids.clone(),
                self.project_id,
                hash_key.clone(),
            )
            .await
            {
                error!("Failed to set feature flag hash key overrides: {:?}", e);
                let reason = parse_exception_for_prometheus_label(&e);
                inc(
                    FLAG_EVALUATION_ERROR_COUNTER,
                    &[("reason".to_string(), reason.to_string())],
                    1,
                );
                return (None, true);
            }
            writing_hash_key_override = true;
        }

        inc(
            FLAG_HASH_KEY_WRITES_COUNTER,
            &[(
                "successful_write".to_string(),
                writing_hash_key_override.to_string(),
            )],
            1,
        );

        match get_feature_flag_hash_key_overrides(
            self.reader.clone(),
            self.team_id,
            target_distinct_ids,
        )
        .await
        {
            Ok(overrides) => (Some(overrides), false),
            Err(e) => {
                error!("Failed to get feature flag hash key overrides: {:?}", e);
                let reason = parse_exception_for_prometheus_label(&e);
                common_metrics::inc(
                    FLAG_EVALUATION_ERROR_COUNTER,
                    &[("reason".to_string(), reason.to_string())],
                    1,
                );
                (None, true)
            }
        }
    }

    /// Evaluates and caches static cohort memberships for the current person.
    /// This should be called once per request to avoid multiple DB lookups.
    async fn evaluate_and_cache_static_cohorts(
        &mut self,
        cohorts: &[Cohort],
    ) -> Result<HashMap<CohortId, bool>, FlagError> {
        // Skip if we've already cached the results
        if self.flag_evaluation_state.static_cohort_matches.is_some() {
            return Ok(self
                .flag_evaluation_state
                .static_cohort_matches
                .clone()
                .unwrap());
        }

        let person_id = self.get_person_id().await?;
        let static_cohorts: Vec<_> = cohorts.iter().filter(|c| c.is_static).collect();

        if static_cohorts.is_empty() {
            // Cache empty map to indicate we've checked
            self.flag_evaluation_state.static_cohort_matches = Some(HashMap::new());
            return Ok(HashMap::new());
        }

        let results = evaluate_static_cohorts(
            self.reader.clone(),
            person_id,
            static_cohorts.iter().map(|c| c.id).collect(),
        )
        .await?
        .into_iter()
        .collect::<HashMap<_, _>>();

        self.flag_evaluation_state.static_cohort_matches = Some(results.clone());
        Ok(results.clone())
    }

    /// Evaluates cohort filters using cached static cohort results where possible.
    /// For dynamic cohorts, evaluates them based on the provided properties.
    pub async fn evaluate_cohort_filters(
        &mut self,
        cohort_property_filters: &[PropertyFilter],
        target_properties: &HashMap<String, Value>,
    ) -> Result<bool, FlagError> {
        // At the start of the request, fetch all of the cohorts for the project from the cache
        let cohorts = self.cohort_cache.get_cohorts(self.project_id).await?;

        // Get cached static cohort results or evaluate them if not cached
        let static_cohort_matches = match self.flag_evaluation_state.static_cohort_matches.as_ref()
        {
            Some(matches) => matches.clone(),
            None => self.evaluate_and_cache_static_cohorts(&cohorts).await?,
        };

        // Store all cohort match results, starting with static cohort results
        let mut cohort_matches = static_cohort_matches;

        // For any cohorts not yet evaluated (i.e., dynamic ones), evaluate them
        for filter in cohort_property_filters {
            let cohort_id = filter
                .get_cohort_id()
                .ok_or(FlagError::CohortFiltersParsingError)?;

            if let Entry::Vacant(e) = cohort_matches.entry(cohort_id) {
                let match_result =
                    evaluate_dynamic_cohorts(cohort_id, target_properties, &cohorts)?;
                e.insert(match_result);
            }
        }

        // Apply cohort membership logic (IN|NOT_IN) to the cohort match results
        apply_cohort_membership_logic(cohort_property_filters, &cohort_matches)
    }

    /// Evaluates feature flags with property and hash key overrides.
    ///
    /// This function evaluates feature flags in two steps:
    /// 1. First, it evaluates flags that can be computed using only the provided property overrides
    /// 2. Then, for remaining flags that need database properties, it fetches and caches those properties
    ///    before evaluating those flags
    pub async fn evaluate_flags_with_overrides(
        &mut self,
        feature_flags: FeatureFlagList,
        person_property_overrides: Option<HashMap<String, Value>>,
        group_property_overrides: Option<HashMap<String, HashMap<String, Value>>>,
        hash_key_overrides: Option<HashMap<String, String>>,
    ) -> FlagsResponse {
        let mut errors_while_computing_flags = false;
        let mut flag_details_map = HashMap::new();
        let mut flags_needing_db_properties = Vec::new();

        // Step 1: Evaluate flags with locally computable property overrides first
        for flag in &feature_flags.flags {
            // we shouldn't have any disabled or deleted flags (the query should filter them out),
            // but just in case, we skip them here
            if !flag.active || flag.deleted {
                continue;
            }

            let property_override_match_timer =
                common_metrics::timing_guard(FLAG_LOCAL_PROPERTY_OVERRIDE_MATCH_TIME, &[]);

            match self
                .match_flag_with_property_overrides(
                    flag,
                    &person_property_overrides,
                    &group_property_overrides,
                    hash_key_overrides.clone(),
                )
                .await
            {
                Ok(Some(flag_match)) => {
                    flag_details_map
                        .insert(flag.key.clone(), FlagDetails::create(flag, &flag_match));
                }
                Ok(None) => {
                    flags_needing_db_properties.push(flag.clone());
                }
                Err(e) => {
                    errors_while_computing_flags = true;
                    error!(
                        "Error evaluating feature flag '{}' with overrides for distinct_id '{}': {:?}",
                        flag.key, self.distinct_id, e
                    );
                    let reason = parse_exception_for_prometheus_label(&e);
                    inc(
                        FLAG_EVALUATION_ERROR_COUNTER,
                        &[("reason".to_string(), reason.to_string())],
                        1,
                    );
                }
            }
            property_override_match_timer
                .label(
                    "outcome",
                    if errors_while_computing_flags {
                        "error"
                    } else {
                        "success"
                    },
                )
                .fin();
        }

        // Step 2: Prepare evaluation data for remaining flags
        if !flags_needing_db_properties.is_empty() {
            if let Err(e) = self
                .prepare_flag_evaluation_state(&flags_needing_db_properties)
                .await
            {
                errors_while_computing_flags = true;
                let reason = parse_exception_for_prometheus_label(&e);
                for flag in flags_needing_db_properties {
                    flag_details_map
                        .insert(flag.key.clone(), FlagDetails::create_error(&flag, reason));
                }
                return FlagsResponse::new(errors_while_computing_flags, flag_details_map, None);
            }

            // Step 3: Evaluate remaining flags with cached properties
            let flag_get_match_timer = common_metrics::timing_guard(FLAG_GET_MATCH_TIME, &[]);
            for flag in flags_needing_db_properties {
                match self
                    .get_match(&flag, None, hash_key_overrides.clone())
                    .await
                {
                    Ok(flag_match) => {
                        flag_details_map
                            .insert(flag.key.clone(), FlagDetails::create(&flag, &flag_match));
                    }
                    Err(e) => {
                        errors_while_computing_flags = true;
                        // TODO add posthog error tracking
                        error!(
                            "Error evaluating feature flag '{}' for distinct_id '{}': {:?}",
                            flag.key, self.distinct_id, e
                        );
                        let reason = parse_exception_for_prometheus_label(&e);
                        inc(
                            FLAG_EVALUATION_ERROR_COUNTER,
                            &[("reason".to_string(), reason.to_string())],
                            1,
                        );
                        flag_details_map
                            .insert(flag.key.clone(), FlagDetails::create_error(&flag, reason));
                    }
                }
            }
            flag_get_match_timer
                .label(
                    "outcome",
                    if errors_while_computing_flags {
                        "error"
                    } else {
                        "success"
                    },
                )
                .fin();
        }

        FlagsResponse::new(errors_while_computing_flags, flag_details_map, None)
    }

    /// Matches a feature flag with property overrides.
    ///
    /// This function attempts to match a feature flag using either group or person property overrides,
    /// depending on whether the flag is group-based or person-based. It first collects all property
    /// filters from the flag's conditions, then retrieves the appropriate overrides, and finally
    /// attempts to match the flag using these overrides.
    async fn match_flag_with_property_overrides(
        &mut self,
        flag: &FeatureFlag,
        person_property_overrides: &Option<HashMap<String, Value>>,
        group_property_overrides: &Option<HashMap<String, HashMap<String, Value>>>,
        hash_key_overrides: Option<HashMap<String, String>>,
    ) -> Result<Option<FeatureFlagMatch>, FlagError> {
        let flag_property_filters: Vec<PropertyFilter> = flag
            .get_conditions()
            .iter()
            .flat_map(|c| c.properties.clone().unwrap_or_default())
            .collect();

        let overrides = match flag.get_group_type_index() {
            Some(group_type_index) => {
                self.get_group_overrides(
                    group_type_index,
                    group_property_overrides,
                    &flag_property_filters,
                )
                .await?
            }
            None => self.get_person_overrides(person_property_overrides, &flag_property_filters),
        };

        match overrides {
            Some(props) => self
                .get_match(flag, Some(props), hash_key_overrides)
                .await
                .map(Some),
            None => Ok(None),
        }
    }

    /// Retrieves group overrides for a specific group type index.
    ///
    /// This function attempts to find and return property overrides for a given group type.
    /// It first maps the group type index to a group type, then checks if there are any
    /// overrides for that group type in the provided group property overrides.
    async fn get_group_overrides(
        &mut self,
        group_type_index: GroupTypeIndex,
        group_property_overrides: &Option<HashMap<String, HashMap<String, Value>>>,
        flag_property_filters: &[PropertyFilter],
    ) -> Result<Option<HashMap<String, Value>>, FlagError> {
        let index_to_type_map = self
            .group_type_mapping_cache
            .group_type_index_to_group_type_map()
            .await?;

        if let Some(group_type) = index_to_type_map.get(&group_type_index) {
            if let Some(group_overrides) = group_property_overrides {
                if let Some(group_overrides_by_type) = group_overrides.get(group_type) {
                    return Ok(locally_computable_property_overrides(
                        &Some(group_overrides_by_type.clone()),
                        flag_property_filters,
                    ));
                }
            }
        }

        Ok(None)
    }

    /// Retrieves person overrides for feature flag evaluation.
    ///
    /// This function attempts to find and return property overrides for a person.
    /// It uses the provided person property overrides and filters them based on
    /// the property filters defined in the feature flag.
    fn get_person_overrides(
        &self,
        person_property_overrides: &Option<HashMap<String, Value>>,
        flag_property_filters: &[PropertyFilter],
    ) -> Option<HashMap<String, Value>> {
        person_property_overrides.as_ref().and_then(|overrides| {
            locally_computable_property_overrides(&Some(overrides.clone()), flag_property_filters)
        })
    }

    /// Determines if a feature flag matches for the current context.
    ///
    /// This method evaluates the conditions of a feature flag to determine if it should be enabled,
    /// and if so, which variant (if any) should be applied. It follows these steps:
    ///
    /// 1. Check if there's a valid hashed identifier for the flag.
    /// 2. Evaluate any super conditions that might override normal conditions.
    /// 3. Sort and evaluate each condition, prioritizing those with variant overrides.
    /// 4. For each matching condition, determine the appropriate variant and payload.
    /// 5. Return the result of the evaluation, including match status, variant, reason, and payload.
    ///
    /// The method also keeps track of the highest priority match reason and index,
    /// which are used even if no conditions ultimately match.
    pub async fn get_match(
        &mut self,
        flag: &FeatureFlag,
        property_overrides: Option<HashMap<String, Value>>,
        hash_key_overrides: Option<HashMap<String, String>>,
    ) -> Result<FeatureFlagMatch, FlagError> {
        if self
            .hashed_identifier(flag, hash_key_overrides.clone())
            .await?
            .is_empty()
        {
            return Ok(FeatureFlagMatch {
                matches: false,
                variant: None,
                reason: FeatureFlagMatchReason::NoGroupType,
                condition_index: None,
                payload: None,
            });
        }

        let mut highest_match = FeatureFlagMatchReason::NoConditionMatch;
        let mut highest_index = None;

        // Evaluate any super conditions first
        if let Some(super_groups) = &flag.filters.super_groups {
            if !super_groups.is_empty() {
                let super_condition_evaluation = self
                    .is_super_condition_match(
                        flag,
                        property_overrides.clone(),
                        hash_key_overrides.clone(),
                    )
                    .await?;

                if super_condition_evaluation.should_evaluate {
                    let payload = self.get_matching_payload(None, flag);
                    return Ok(FeatureFlagMatch {
                        matches: super_condition_evaluation.is_match,
                        variant: None,
                        reason: super_condition_evaluation.reason,
                        condition_index: Some(0),
                        payload,
                    });
                } // if no match, continue to normal conditions
            }
        }

        // Match for holdout super condition
        // TODO: Flags shouldn't have both super_groups and holdout_groups
        // TODO: Validate only multivariant flags to have holdout groups. I could make this implicit by reusing super_groups but
        // this will shoot ourselves in the foot when we extend early access to support variants as well.
        // TODO: Validate holdout variant should have 0% default rollout %?
        // TODO: All this validation we need to do suggests the modelling is imperfect here. Carrying forward for now, we'll only enable
        // in beta, and potentially rework representation before rolling out to everyone. Probably the problem is holdout groups are an
        // experiment level concept that applies across experiments, and we are creating a feature flag level primitive to handle it.
        // Validating things like the variant name is the same across all flags, rolled out to 0%, has the same correct conditions is a bit of
        // a pain here. But I'm not sure if feature flags should indeed know all this info. It's fine for them to just work with what they're given.
        if let Some(holdout_groups) = &flag.filters.holdout_groups {
            if !holdout_groups.is_empty() {
                let (is_match, holdout_value, evaluation_reason) =
                    self.is_holdout_condition_match(flag).await?;
                if is_match {
                    let payload = self.get_matching_payload(holdout_value.as_deref(), flag);
                    return Ok(FeatureFlagMatch {
                        matches: true,
                        variant: holdout_value,
                        reason: evaluation_reason,
                        condition_index: None,
                        payload,
                    });
                }
            }
        }
        // Sort conditions with variant overrides to the top so that we can evaluate them first
        let mut sorted_conditions: Vec<(usize, &FlagGroupType)> =
            flag.get_conditions().iter().enumerate().collect();

        sorted_conditions
            .sort_by_key(|(_, condition)| if condition.variant.is_some() { 0 } else { 1 });

        let condition_timer = common_metrics::timing_guard(FLAG_EVALUATE_ALL_CONDITIONS_TIME, &[]);
        for (index, condition) in sorted_conditions {
            let (is_match, reason) = self
                .is_condition_match(
                    flag,
                    condition,
                    property_overrides.clone(),
                    hash_key_overrides.clone(),
                )
                .await?;

            // Update highest_match and highest_index
            let (new_highest_match, new_highest_index) = self
                .get_highest_priority_match_evaluation(
                    highest_match.clone(),
                    highest_index,
                    reason.clone(),
                    Some(index),
                );
            highest_match = new_highest_match;
            highest_index = new_highest_index;

            if is_match {
                if highest_match == FeatureFlagMatchReason::SuperConditionValue {
                    break; // Exit early if we've found a super condition match
                }

                // Check for variant override in the condition
                let variant = if let Some(variant_override) = &condition.variant {
                    // Check if the override is a valid variant
                    if flag
                        .get_variants()
                        .iter()
                        .any(|v| &v.key == variant_override)
                    {
                        Some(variant_override.clone())
                    } else {
                        // If override isn't valid, fall back to computed variant
                        self.get_matching_variant(flag, hash_key_overrides.clone())
                            .await?
                    }
                } else {
                    // No override, use computed variant
                    self.get_matching_variant(flag, hash_key_overrides.clone())
                        .await?
                };
                let payload = self.get_matching_payload(variant.as_deref(), flag);

                return Ok(FeatureFlagMatch {
                    matches: true,
                    variant,
                    reason: highest_match,
                    condition_index: highest_index,
                    payload,
                });
            }
        }

        condition_timer.label("outcome", "success").fin();
        // Return with the highest_match reason and index even if no conditions matched
        Ok(FeatureFlagMatch {
            matches: false,
            variant: None,
            reason: highest_match,
            condition_index: highest_index,
            payload: None,
        })
    }

    /// This function determines the highest priority match evaluation for feature flag conditions.
    /// It compares the current match reason with a new match reason and returns the higher priority one.
    /// The priority is determined by the ordering of FeatureFlagMatchReason variants.
    /// It's used to keep track of the most significant reason why a flag matched or didn't match,
    /// especially useful when multiple conditions are evaluated.
    fn get_highest_priority_match_evaluation(
        &self,
        current_match: FeatureFlagMatchReason,
        current_index: Option<usize>,
        new_match: FeatureFlagMatchReason,
        new_index: Option<usize>,
    ) -> (FeatureFlagMatchReason, Option<usize>) {
        if current_match <= new_match {
            (new_match, new_index)
        } else {
            (current_match, current_index)
        }
    }

    /// Check if a condition matches for a feature flag.
    ///
    /// This function evaluates a specific condition of a feature flag to determine if it should be enabled.
    /// It first checks if the condition has any property filters. If not, it performs a rollout check.
    /// Otherwise, it fetches the relevant properties and checks if they match the condition's filters.
    /// The function returns a tuple indicating whether the condition matched and the reason for the match.
    async fn is_condition_match(
        &mut self,
        feature_flag: &FeatureFlag,
        condition: &FlagGroupType,
        property_overrides: Option<HashMap<String, Value>>,
        hash_key_overrides: Option<HashMap<String, String>>,
    ) -> Result<(bool, FeatureFlagMatchReason), FlagError> {
        let rollout_percentage = condition.rollout_percentage.unwrap_or(100.0);

        if let Some(flag_property_filters) = &condition.properties {
            if flag_property_filters.is_empty() {
                return self
                    .check_rollout(feature_flag, rollout_percentage, hash_key_overrides)
                    .await;
            }

            // Separate cohort and non-cohort filters
            let (cohort_filters, non_cohort_filters): (Vec<PropertyFilter>, Vec<PropertyFilter>) =
                flag_property_filters
                    .iter()
                    .cloned()
                    .partition(|prop| prop.is_cohort());

            // Get the properties we need to check for in this condition match from the flag + any overrides
            let person_or_group_properties = self
                .get_properties_to_check(feature_flag, property_overrides, &non_cohort_filters)
                .await?;

            // Evaluate non-cohort filters first, since they're cheaper to evaluate and we can return early if they don't match
            if !all_properties_match(&non_cohort_filters, &person_or_group_properties) {
                return Ok((false, FeatureFlagMatchReason::NoConditionMatch));
            }

            // Evaluate cohort filters, if any.
            if !cohort_filters.is_empty() {
                // Get the person ID for the current distinct ID – this value should be cached at this point, and if we can't get it we return false.
                if !self
                    .evaluate_cohort_filters(&cohort_filters, &person_or_group_properties)
                    .await?
                {
                    return Ok((false, FeatureFlagMatchReason::NoConditionMatch));
                }
            }
        }

        self.check_rollout(feature_flag, rollout_percentage, hash_key_overrides)
            .await
    }

    /// Get properties to check for a feature flag.
    ///
    /// This function determines which properties to check based on the feature flag's group type index.
    /// If the flag is group-based, it fetches group properties; otherwise, it fetches person properties.
    async fn get_properties_to_check(
        &mut self,
        feature_flag: &FeatureFlag,
        property_overrides: Option<HashMap<String, Value>>,
        flag_property_filters: &[PropertyFilter],
    ) -> Result<HashMap<String, Value>, FlagError> {
        if let Some(group_type_index) = feature_flag.get_group_type_index() {
            self.get_group_properties(group_type_index, property_overrides, flag_property_filters)
                .await
        } else {
            self.get_person_properties(property_overrides, flag_property_filters)
                .await
        }
    }

    /// Get group properties from overrides, cache or database.
    ///
    /// This function attempts to retrieve group properties either from a cache or directly from the database.
    /// It first checks if there are any locally computable property overrides. If so, it returns those.
    /// Otherwise, it fetches the properties from the cache or database and returns them.
    async fn get_group_properties(
        &mut self,
        group_type_index: GroupTypeIndex,
        property_overrides: Option<HashMap<String, Value>>,
        flag_property_filters: &[PropertyFilter],
    ) -> Result<HashMap<String, Value>, FlagError> {
        if let Some(overrides) =
            locally_computable_property_overrides(&property_overrides, flag_property_filters)
        {
            Ok(overrides)
        } else {
            self.get_group_properties_from_cache_or_db(group_type_index)
                .await
        }
    }

    /// Retrieves the `PersonId` from the properties cache.
    /// If the cache does not contain a `PersonId`, it fetches it from the database
    /// and updates the cache accordingly.
    async fn get_person_id(&mut self) -> Result<PersonId, FlagError> {
        match self.flag_evaluation_state.person_id {
            Some(id) => {
                inc(
                    PROPERTY_CACHE_HITS_COUNTER,
                    &[("type".to_string(), "person_id".to_string())],
                    1,
                );
                Ok(id)
            }
            None => {
                let id = self.get_person_id_from_db().await?;
                inc(DB_PERSON_PROPERTIES_READS_COUNTER, &[], 1);
                self.flag_evaluation_state.person_id = Some(id);
                Ok(id)
            }
        }
    }

    /// Fetches the `PersonId` from the database based on the current `distinct_id` and `team_id`.
    /// This method is called when the `PersonId` is not present in the properties cache.
    async fn get_person_id_from_db(&mut self) -> Result<PersonId, FlagError> {
        let reader = self.reader.clone();
        let distinct_id = self.distinct_id.clone();
        let team_id = self.team_id;
        fetch_person_properties_from_db(reader, distinct_id, team_id)
            .await
            .map(|(_, person_id)| person_id)
    }

    /// Get person properties from overrides, cache or database.
    ///
    /// This function attempts to retrieve person properties either from a cache or directly from the database.
    /// It first checks if there are any locally computable property overrides. If so, it returns those.
    /// Otherwise, it fetches the properties from the cache or database and returns them.
    async fn get_person_properties(
        &mut self,
        property_overrides: Option<HashMap<String, Value>>,
        flag_property_filters: &[PropertyFilter],
    ) -> Result<HashMap<String, Value>, FlagError> {
        if let Some(overrides) =
            locally_computable_property_overrides(&property_overrides, flag_property_filters)
        {
            Ok(overrides)
        } else {
            match self.get_person_properties_from_cache_or_db().await {
                Ok(props) => Ok(props),
                Err(FlagError::PersonNotFound) => Ok(HashMap::new()), // NB: If we can't find a person ID associated with the distinct ID, return an empty map
                Err(e) => Err(e),
            }
        }
    }

    async fn is_holdout_condition_match(
        &mut self,
        flag: &FeatureFlag,
    ) -> Result<(bool, Option<String>, FeatureFlagMatchReason), FlagError> {
        // TODO: Right now holdout conditions only support basic rollout %s, and not property overrides.

        if let Some(holdout_groups) = &flag.filters.holdout_groups {
            if !holdout_groups.is_empty() {
                let condition = &holdout_groups[0];
                // TODO: Check properties and match based on them

                if condition
                    .properties
                    .as_ref()
                    .map_or(false, |p| !p.is_empty())
                {
                    return Ok((false, None, FeatureFlagMatchReason::NoConditionMatch));
                }

                let rollout_percentage = condition.rollout_percentage;

                if let Some(percentage) = rollout_percentage {
                    if self.get_holdout_hash(flag, None).await? > (percentage / 100.0) {
                        // If hash is greater than percentage, we're OUT of holdout
                        return Ok((false, None, FeatureFlagMatchReason::OutOfRolloutBound));
                    }
                }

                // rollout_percentage is None (=100%), or we are inside holdout rollout bound.
                // Thus, we match. Now get the variant override for the holdout condition.
                let variant = if let Some(variant_override) = condition.variant.as_ref() {
                    variant_override.clone()
                } else {
                    self.get_matching_variant(flag, None)
                        .await?
                        .unwrap_or_else(|| "holdout".to_string())
                };

                return Ok((
                    true,
                    Some(variant),
                    FeatureFlagMatchReason::HoldoutConditionValue,
                ));
            }
        }
        Ok((false, None, FeatureFlagMatchReason::NoConditionMatch))
    }

    /// Check if a super condition matches for a feature flag.
    ///
    /// This function evaluates the super conditions of a feature flag to determine if any of them should be enabled.
    /// It first checks if there are any super conditions. If so, it evaluates the first condition.
    /// The function returns a struct indicating whether a super condition should be evaluated,
    /// whether it matches if evaluated, and the reason for the match.
    async fn is_super_condition_match(
        &mut self,
        feature_flag: &FeatureFlag,
        property_overrides: Option<HashMap<String, Value>>,
        hash_key_overrides: Option<HashMap<String, String>>,
    ) -> Result<SuperConditionEvaluation, FlagError> {
        if let Some(first_condition) = feature_flag
            .filters
            .super_groups
            .as_ref()
            .and_then(|sc| sc.first())
        {
            // Need to fetch person properties to check super conditions.  If these properties are already locally computable,
            // we don't need to fetch from the database, but if they aren't we need to fetch from the database and then we'll cache them.
            let person_properties = self
                .get_person_properties(
                    property_overrides,
                    first_condition.properties.as_deref().unwrap_or(&[]),
                )
                .await?;

            let has_relevant_super_condition_properties =
                first_condition.properties.as_ref().map_or(false, |props| {
                    props
                        .iter()
                        .any(|prop| person_properties.contains_key(&prop.key))
                });

            let (is_match, _) = self
                .is_condition_match(
                    feature_flag,
                    first_condition,
                    Some(person_properties),
                    hash_key_overrides,
                )
                .await?;

            if has_relevant_super_condition_properties {
                return Ok(SuperConditionEvaluation {
                    should_evaluate: true,
                    is_match,
                    reason: FeatureFlagMatchReason::SuperConditionValue,
                });
                // If there is a super condition evaluation, return early with those results.
                // The reason is super condition value because we're not evaluating the rest of the conditions.
            }
        }

        Ok(SuperConditionEvaluation {
            should_evaluate: false,
            is_match: false,
            reason: FeatureFlagMatchReason::NoConditionMatch,
        })
    }

    /// Get group properties from cache or database.
    ///
    /// This function attempts to retrieve group properties either from a cache or directly from the database.
    /// It first checks if the properties are already cached. If so, it returns those.
    /// Otherwise, it fetches the properties from the database and caches them.
    async fn get_group_properties_from_cache_or_db(
        &mut self,
        group_type_index: GroupTypeIndex,
    ) -> Result<HashMap<String, Value>, FlagError> {
        // check if the properties are already cached, if so return them
        if let Some(properties) = self
            .flag_evaluation_state
            .group_properties
            .get(&group_type_index)
        {
            inc(
                PROPERTY_CACHE_HITS_COUNTER,
                &[("type".to_string(), "group_properties".to_string())],
                1,
            );
            let mut result = HashMap::new();
            result.clone_from(properties);
            return Ok(result);
        }

        inc(
            PROPERTY_CACHE_MISSES_COUNTER,
            &[("type".to_string(), "group_properties".to_string())],
            1,
        );

        let reader = self.reader.clone();
        let team_id = self.team_id;
        // groups looks like this {"project": "project_123"}
        // and then the group type index looks like this {"project": 1}
        // so I want my group keys to look like this ["project_123"],
        // but they need to be aware of the different group types
        // Retrieve group_type_name using group_type_index from the cache
        let group_type_mapping = self
            .group_type_mapping_cache
            .group_type_index_to_group_type_map()
            .await?;
        let group_type_name = match group_type_mapping.get(&group_type_index) {
            Some(name) => name.clone(),
            None => {
                error!(
                    "No group_type_name found for group_type_index {}",
                    group_type_index
                );
                return Err(FlagError::NoGroupTypeMappings);
            }
        };

        // Retrieve the corresponding group_key from self.groups using group_type_name
        let group_key = match self.groups.get(&group_type_name) {
            Some(Value::String(key)) => key.clone(),
            Some(_) => {
                error!(
                    "Group key for group_type_name '{}' is not a string",
                    group_type_name
                );
                return Err(FlagError::NoGroupTypeMappings);
            }
            None => {
                // If there's no group_key provided for this group_type_name, we consider that there are no properties to fetch
                return Ok(HashMap::new());
            }
        };
        let db_properties =
            fetch_group_properties_from_db(reader, team_id, group_type_index, group_key).await?;

        inc(DB_GROUP_PROPERTIES_READS_COUNTER, &[], 1);

        // once the properties are fetched, cache them so we don't need to fetch again in a given request
        self.flag_evaluation_state
            .group_properties
            .insert(group_type_index, db_properties.clone());

        Ok(db_properties)
    }

    /// Get person properties from cache or database.
    ///
    /// This function attempts to retrieve person properties either from a cache or directly from the database.
    /// It first checks if the properties are already cached. If so, it returns those.
    /// Otherwise, it fetches the properties from the database and caches them.
    async fn get_person_properties_from_cache_or_db(
        &mut self,
    ) -> Result<HashMap<String, Value>, FlagError> {
        // check if the properties are already cached, if so return them
        if let Some(properties) = &self.flag_evaluation_state.person_properties {
            inc(
                PROPERTY_CACHE_HITS_COUNTER,
                &[("type".to_string(), "person_properties".to_string())],
                1,
            );
            let mut result = HashMap::new();
            result.clone_from(properties);
            return Ok(result);
        }

        inc(
            PROPERTY_CACHE_MISSES_COUNTER,
            &[("type".to_string(), "person_properties".to_string())],
            1,
        );

        let reader = self.reader.clone();
        let distinct_id = self.distinct_id.clone();
        let team_id = self.team_id;
        let (db_properties, person_id) =
            fetch_person_properties_from_db(reader, distinct_id, team_id).await?;

        inc(DB_PERSON_PROPERTIES_READS_COUNTER, &[], 1);

        // once the properties and person ID are fetched, cache them so we don't need to fetch again in a given request
        self.flag_evaluation_state.person_properties = Some(db_properties.clone());
        self.flag_evaluation_state.person_id = Some(person_id);

        Ok(db_properties)
    }

    /// Get hashed identifier for a feature flag.
    ///
    /// This function generates a hashed identifier for a feature flag based on the feature flag's group type index.
    /// If the feature flag is group-based, it fetches the group key; otherwise, it uses the distinct ID.
    async fn hashed_identifier(
        &mut self,
        feature_flag: &FeatureFlag,
        hash_key_overrides: Option<HashMap<String, String>>,
    ) -> Result<String, FlagError> {
        if let Some(group_type_index) = feature_flag.get_group_type_index() {
            // Group-based flag
            let group_key = self
                .group_type_mapping_cache
                .group_type_index_to_group_type_map()
                .await?
                .get(&group_type_index)
                .and_then(|group_type_name| self.groups.get(group_type_name))
                .and_then(|group_key_value| group_key_value.as_str())
                // NB: we currently use empty string ("") as the hashed identifier for group flags without a group key,
                // and I don't want to break parity with the old service since I don't want the hash values to change
                .unwrap_or("")
                .to_string();

            Ok(group_key)
        } else {
            // Person-based flag
            // Use hash key overrides for experience continuity
            if let Some(hash_key_override) = hash_key_overrides
                .as_ref()
                .and_then(|h| h.get(&feature_flag.key))
            {
                Ok(hash_key_override.clone())
            } else {
                Ok(self.distinct_id.clone())
            }
        }
    }

    /// This function takes a identifier and a feature flag key and returns a float between 0 and 1.
    /// Given the same identifier and key, it'll always return the same float. These floats are
    /// uniformly distributed between 0 and 1, so if we want to show this feature to 20% of traffic
    /// we can do _hash(key, identifier) < 0.2
    async fn get_hash(
        &mut self,
        feature_flag: &FeatureFlag,
        salt: &str,
        hash_key_overrides: Option<HashMap<String, String>>,
    ) -> Result<f64, FlagError> {
        let hashed_identifier = self
            .hashed_identifier(feature_flag, hash_key_overrides)
            .await?;
        if hashed_identifier.is_empty() {
            // Return a hash value that will make the flag evaluate to false; since we
            // can't evaluate a flag without an identifier.
            return Ok(0.0); // NB: A flag with 0.0 hash will always evaluate to false
        }

        calculate_hash(&format!("{}.", feature_flag.key), &hashed_identifier, salt).await
    }

    async fn get_holdout_hash(
        &mut self,
        feature_flag: &FeatureFlag,
        salt: Option<&str>,
    ) -> Result<f64, FlagError> {
        let hashed_identifier = self.hashed_identifier(feature_flag, None).await?;
        let hash = calculate_hash("holdout-", &hashed_identifier, salt.unwrap_or("")).await?;
        Ok(hash)
    }

    /// Check if a feature flag should be shown based on its rollout percentage.
    ///
    /// This function determines if a feature flag should be shown to a user based on the flag's rollout percentage.
    /// It first calculates a hash of the feature flag's identifier and compares it to the rollout percentage.
    /// If the hash value is less than or equal to the rollout percentage, the flag is shown; otherwise, it is not.
    /// The function returns a tuple indicating whether the flag matched and the reason for the match.
    async fn check_rollout(
        &mut self,
        feature_flag: &FeatureFlag,
        rollout_percentage: f64,
        hash_key_overrides: Option<HashMap<String, String>>,
    ) -> Result<(bool, FeatureFlagMatchReason), FlagError> {
        let hash = self.get_hash(feature_flag, "", hash_key_overrides).await?;
        if rollout_percentage == 100.0 || hash <= (rollout_percentage / 100.0) {
            Ok((true, FeatureFlagMatchReason::ConditionMatch))
        } else {
            Ok((false, FeatureFlagMatchReason::OutOfRolloutBound))
        }
    }

    /// This function takes a feature flag and returns the key of the variant that should be shown to the user.
    async fn get_matching_variant(
        &mut self,
        feature_flag: &FeatureFlag,
        hash_key_overrides: Option<HashMap<String, String>>,
    ) -> Result<Option<String>, FlagError> {
        let hash = self
            .get_hash(feature_flag, "variant", hash_key_overrides)
            .await?;
        let mut cumulative_percentage = 0.0;

        for variant in feature_flag.get_variants() {
            cumulative_percentage += variant.rollout_percentage / 100.0;
            if hash < cumulative_percentage {
                return Ok(Some(variant.key.clone()));
            }
        }
        Ok(None)
    }

    /// Get matching payload for a feature flag.
    ///
    /// This function retrieves the payload associated with a matching variant of a feature flag.
    /// It takes the matched variant key and the feature flag itself as inputs and returns the payload.
    fn get_matching_payload(
        &self,
        match_variant: Option<&str>,
        feature_flag: &FeatureFlag,
    ) -> Option<serde_json::Value> {
        let variant = match_variant.unwrap_or("true");
        feature_flag.get_payload(variant)
    }

    /// Prepares all database-sourced data needed for flag evaluation.
    /// This includes:
    /// - Static cohort memberships
    /// - Group type mappings
    /// - Person and group properties
    ///
    /// The data is cached in FlagEvaluationState to avoid repeated DB lookups
    /// during subsequent flag evaluations.
    async fn prepare_flag_evaluation_state(
        &mut self,
        flags: &[FeatureFlag],
    ) -> Result<(), FlagError> {
        // First, prepare cohort data since other evaluations may depend on it
        let cohort_timer = common_metrics::timing_guard(FLAG_STATIC_COHORT_DB_EVALUATION_TIME, &[]);
        self.prepare_cohort_data().await?;
        cohort_timer.fin();

        // Then prepare group mappings and properties
        let group_timer = common_metrics::timing_guard(FLAG_GROUP_FETCH_TIME, &[]);
        let group_data = self.prepare_group_data(flags).await?;
        group_timer.fin();

        // Finally fetch and cache all properties (the timer is included in prepare_properties_data, so we don't need to add it here)
        self.prepare_properties_data(&group_data).await?;
        Ok(())
    }

    /// Fetches and caches static cohort memberships
    async fn prepare_cohort_data(&mut self) -> Result<(), FlagError> {
        let cohorts = self.cohort_cache.get_cohorts(self.project_id).await?;
        self.evaluate_and_cache_static_cohorts(&cohorts).await?;
        Ok(())
    }

    /// Analyzes flags and prepares required group type data
    async fn prepare_group_data(
        &mut self,
        flags: &[FeatureFlag],
    ) -> Result<GroupEvaluationData, FlagError> {
        // Extract required group type indexes from flags
        let type_indexes: HashSet<GroupTypeIndex> = flags
            .iter()
            .filter_map(|flag| flag.get_group_type_index())
            .collect();

        // Map group names to group_type_index and group_keys
        let group_type_to_key_map: HashMap<GroupTypeIndex, String> = self
            .groups
            .iter()
            .filter_map(|(group_type, group_key_value)| {
                let group_key = group_key_value.as_str()?.to_string();
                self.group_type_mapping_cache
                    .group_types_to_indexes
                    .get(group_type)
                    .cloned()
                    .map(|group_type_index| (group_type_index, group_key))
            })
            .collect();

        // Extract group_keys that are relevant to the required group_type_indexes
        let keys: HashSet<String> = group_type_to_key_map
            .iter()
            .filter_map(|(group_type_index, group_key)| {
                if type_indexes.contains(group_type_index) {
                    Some(group_key.clone())
                } else {
                    None
                }
            })
            .collect();

        Ok(GroupEvaluationData { type_indexes, keys })
    }

    /// Fetches and caches all required properties and times the operation
    async fn prepare_properties_data(
        &mut self,
        group_data: &GroupEvaluationData,
    ) -> Result<(), FlagError> {
        let db_fetch_timer = common_metrics::timing_guard(FLAG_DB_PROPERTIES_FETCH_TIME, &[]);

        match fetch_and_locally_cache_all_relevant_properties(
            &mut self.flag_evaluation_state,
            self.reader.clone(),
            self.distinct_id.clone(),
            self.team_id,
            &group_data.type_indexes,
            &group_data.keys,
        )
        .await
        {
            Ok(_) => {
                inc(DB_PERSON_AND_GROUP_PROPERTIES_READS_COUNTER, &[], 1);
                db_fetch_timer.label("outcome", "success").fin();
                Ok(())
            }
            Err(e) => {
                error!("Error fetching properties: {:?}", e);
                db_fetch_timer.label("outcome", "error").fin();
                Err(e)
            }
        }
    }
}

pub async fn calculate_hash(
    prefix: &str,
    hashed_identifier: &str,
    salt: &str,
) -> Result<f64, FlagError> {
    let hash_key = format!("{}{}{}", prefix, hashed_identifier, salt);
    let mut hasher = Sha1::new();
    hasher.update(hash_key.as_bytes());
    let result = hasher.finalize();
    // :TRICKY: Convert the first 15 characters of the digest to a hexadecimal string
    let hex_str = result.iter().fold(String::new(), |mut acc, byte| {
        let _ = write!(acc, "{:02x}", byte);
        acc
    })[..15]
        .to_string();
    let hash_val = u64::from_str_radix(&hex_str, 16).unwrap();
    Ok(hash_val as f64 / LONG_SCALE as f64)
}

/// Evaluate static cohort filters by checking if the person is in each cohort.
async fn evaluate_static_cohorts(
    reader: PostgresReader,
    person_id: PersonId,
    cohort_ids: Vec<CohortId>,
) -> Result<Vec<(CohortId, bool)>, FlagError> {
    let mut conn = reader.get_connection().await?;

    let query = r#"
           WITH cohort_membership AS (
               SELECT c.cohort_id, 
                      CASE WHEN pc.cohort_id IS NOT NULL THEN true ELSE false END AS is_member
               FROM unnest($1::integer[]) AS c(cohort_id)
               LEFT JOIN posthog_cohortpeople AS pc
                 ON pc.person_id = $2
                 AND pc.cohort_id = c.cohort_id
           )
           SELECT cohort_id, is_member
           FROM cohort_membership
       "#;

    let rows = sqlx::query(query)
        .bind(&cohort_ids)
        .bind(person_id)
        .fetch_all(&mut *conn)
        .await?;

    let result = rows
        .into_iter()
        .map(|row| {
            let cohort_id: CohortId = row.get("cohort_id");
            let is_member: bool = row.get("is_member");
            (cohort_id, is_member)
        })
        .collect();

    Ok(result)
}

/// Evaluates a dynamic cohort and its dependencies.
/// This uses a topological sort to evaluate dependencies first, which is necessary
/// because a cohort can depend on another cohort, and we need to respect the dependency order.
fn evaluate_dynamic_cohorts(
    initial_cohort_id: CohortId,
    target_properties: &HashMap<String, Value>,
    cohorts: &[Cohort],
) -> Result<bool, FlagError> {
    // First check if this is a static cohort
    let initial_cohort = cohorts
        .iter()
        .find(|c| c.id == initial_cohort_id)
        .ok_or(FlagError::CohortNotFound(initial_cohort_id.to_string()))?;

    // If it's static, we don't need to evaluate dependencies - the membership was already
    // checked in evaluate_static_cohorts and stored in cohort_matches
    if initial_cohort.is_static {
        return Ok(false); // Static cohorts are handled by evaluate_static_cohorts
    }

    let cohort_dependency_graph = build_cohort_dependency_graph(initial_cohort_id, cohorts)?;

    // We need to sort cohorts topologically to ensure we evaluate dependencies before the cohorts that depend on them.
    // For example, if cohort A depends on cohort B, we need to evaluate B first to know if A matches.
    // This also helps detect cycles - if cohort A depends on B which depends on A, toposort will fail.
    let sorted_cohort_ids_as_graph_nodes =
        toposort(&cohort_dependency_graph, None).map_err(|e| {
            FlagError::CohortDependencyCycle(format!("Cyclic dependency detected: {:?}", e))
        })?;

    // Store evaluation results for each cohort in a map, so we can look up whether a cohort matched
    // when evaluating cohorts that depend on it, and also return the final result for the initial cohort
    let mut evaluation_results = HashMap::new();

    // Iterate through the sorted nodes in reverse order (so that we can evaluate dependencies first)
    for node in sorted_cohort_ids_as_graph_nodes.into_iter().rev() {
        let cohort_id = cohort_dependency_graph[node];
        let cohort = cohorts
            .iter()
            .find(|c| c.id == cohort_id)
            .ok_or(FlagError::CohortNotFound(cohort_id.to_string()))?;
        let property_filters = cohort.parse_filters()?;
        let dependencies = cohort.extract_dependencies()?;

        // Check if all dependencies have been met (i.e., previous cohorts matched)
        let dependencies_met = dependencies
            .iter()
            .all(|dep_id| evaluation_results.get(dep_id).copied().unwrap_or(false));

        // If dependencies are not met, mark the current cohort as not matched and continue
        // NB: We don't want to _exit_ here, since the non-matching cohort could be wrapped in a `not_in` operator
        // and we want to evaluate all cohorts to determine if the initial cohort matches.
        if !dependencies_met {
            evaluation_results.insert(cohort_id, false);
            continue;
        }

        // Evaluate all property filters for the current cohort
        let all_filters_match = property_filters
            .iter()
            .all(|filter| match_property(filter, target_properties, false).unwrap_or(false));

        // Store the evaluation result for the current cohort
        evaluation_results.insert(cohort_id, all_filters_match);
    }

    // Retrieve and return the evaluation result for the initial cohort
    evaluation_results
        .get(&initial_cohort_id)
        .copied()
        .ok_or_else(|| FlagError::CohortNotFound(initial_cohort_id.to_string()))
}

/// Apply cohort membership logic (i.e., IN|NOT_IN)
fn apply_cohort_membership_logic(
    cohort_filters: &[PropertyFilter],
    cohort_matches: &HashMap<CohortId, bool>,
) -> Result<bool, FlagError> {
    for filter in cohort_filters {
        let cohort_id = filter
            .get_cohort_id()
            .ok_or(FlagError::CohortFiltersParsingError)?;
        let matches = cohort_matches.get(&cohort_id).copied().unwrap_or(false);
        let operator = filter.operator.unwrap_or(OperatorType::In);

        // Combine the operator logic directly within this method
        let membership_match = match operator {
            OperatorType::In => matches,
            OperatorType::NotIn => !matches,
            // Currently supported operators are IN and NOT IN
            // Any other operator defaults to false
            _ => false,
        };

        // If any filter does not match, return false early
        if !membership_match {
            return Ok(false);
        }
    }
    // All filters matched
    Ok(true)
}

/// Constructs a dependency graph for cohorts.
///
/// Example dependency graph:
/// ```text
///   A    B
///   |   /|
///   |  / |
///   | /  |
///   C    D
///   \   /
///    \ /
///     E
/// ```
/// In this example:
/// - Cohorts A and B are root nodes (no dependencies)
/// - C depends on A and B
/// - D depends on B
/// - E depends on C and D
///
/// The graph is acyclic, which is required for valid cohort dependencies.
fn build_cohort_dependency_graph(
    initial_cohort_id: CohortId,
    cohorts: &[Cohort],
) -> Result<DiGraph<CohortId, ()>, FlagError> {
    let mut graph = DiGraph::new();
    let mut node_map = HashMap::new();
    let mut queue = VecDeque::new();

    let initial_cohort = cohorts
        .iter()
        .find(|c| c.id == initial_cohort_id)
        .ok_or(FlagError::CohortNotFound(initial_cohort_id.to_string()))?;

    if initial_cohort.is_static {
        return Ok(graph);
    }

    // This implements a breadth-first search (BFS) traversal to build a directed graph of cohort dependencies.
    // Starting from the initial cohort, we:
    // 1. Add each cohort as a node in the graph
    // 2. Track visited nodes in a map to avoid duplicates
    // 3. For each cohort, get its dependencies and add directed edges from the cohort to its dependencies
    // 4. Queue up any unvisited dependencies to process their dependencies later
    // This builds up the full dependency graph level by level, which we can later check for cycles
    queue.push_back(initial_cohort_id);
    node_map.insert(initial_cohort_id, graph.add_node(initial_cohort_id));

    while let Some(cohort_id) = queue.pop_front() {
        let cohort = cohorts
            .iter()
            .find(|c| c.id == cohort_id)
            .ok_or(FlagError::CohortNotFound(cohort_id.to_string()))?;
        let dependencies = cohort.extract_dependencies()?;
        for dep_id in dependencies {
            // Retrieve the current node **before** mutable borrowing
            // This is safe because we're not mutating the node map,
            // and it keeps the borrow checker happy
            let current_node = node_map[&cohort_id];
            // Add dependency node if we haven't seen this cohort ID before in our traversal.
            // This happens when we discover a new dependency that wasn't previously
            // encountered while processing other cohorts in the graph.
            let dep_node = node_map
                .entry(dep_id)
                .or_insert_with(|| graph.add_node(dep_id));

            graph.add_edge(current_node, *dep_node, ());

            if !node_map.contains_key(&dep_id) {
                queue.push_back(dep_id);
            }
        }
    }

    if is_cyclic_directed(&graph) {
        return Err(FlagError::CohortDependencyCycle(format!(
            "Cyclic dependency detected starting at cohort {}",
            initial_cohort_id
        )));
    }

    Ok(graph)
}

/// Fetch and locally cache all properties for a given distinct ID and team ID.
///
/// This function fetches both person and group properties for a specified distinct ID and team ID.
/// It updates the properties cache with the fetched properties and returns the result.
async fn fetch_and_locally_cache_all_relevant_properties(
    properties_cache: &mut FlagEvaluationState,
    reader: PostgresReader,
    distinct_id: String,
    team_id: TeamId,
    group_type_indexes: &HashSet<GroupTypeIndex>,
    group_keys: &HashSet<String>,
) -> Result<(), FlagError> {
    let mut conn = reader.as_ref().get_connection().await?;

    let query = r#"
        SELECT
            (
                SELECT "posthog_person"."id"
                FROM "posthog_person"
                INNER JOIN "posthog_persondistinctid"
                    ON "posthog_person"."id" = "posthog_persondistinctid"."person_id"
                WHERE
                    "posthog_persondistinctid"."distinct_id" = $1
                    AND "posthog_persondistinctid"."team_id" = $2
                    AND "posthog_person"."team_id" = $2
                LIMIT 1
            ) AS person_id,
            (
                SELECT "posthog_person"."properties"
                FROM "posthog_person"
                INNER JOIN "posthog_persondistinctid"
                    ON "posthog_person"."id" = "posthog_persondistinctid"."person_id"
                WHERE
                    "posthog_persondistinctid"."distinct_id" = $1
                    AND "posthog_persondistinctid"."team_id" = $2
                    AND "posthog_person"."team_id" = $2
                LIMIT 1
            ) AS person_properties,
            (
                SELECT
                    json_object_agg(
                        "posthog_group"."group_type_index",
                        "posthog_group"."group_properties"
                    )
                FROM "posthog_group"
                WHERE
                    "posthog_group"."team_id" = $2
                    AND "posthog_group"."group_type_index" = ANY($3)
                    AND "posthog_group"."group_key" = ANY($4)
            ) AS group_properties
    "#;

    let group_type_indexes_vec: Vec<GroupTypeIndex> = group_type_indexes.iter().cloned().collect();
    let group_keys_vec: Vec<String> = group_keys.iter().cloned().collect();

    let row: (Option<PersonId>, Option<Value>, Option<Value>) = sqlx::query_as(query)
        .bind(&distinct_id)
        .bind(team_id)
        .bind(&group_type_indexes_vec)
        .bind(&group_keys_vec) // Bind group_keys_vec to $4
        .fetch_optional(&mut *conn)
        .await?
        .unwrap_or((None, None, None));

    let (person_id, person_props, group_props) = row;

    if let Some(person_id) = person_id {
        properties_cache.person_id = Some(person_id);
    }

    if let Some(person_props) = person_props {
        properties_cache.person_properties = Some(
            person_props
                .as_object()
                .unwrap_or(&serde_json::Map::new())
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        );
    }

    if let Some(group_props) = group_props {
        let group_props_map: HashMap<GroupTypeIndex, HashMap<String, Value>> = group_props
            .as_object()
            .unwrap_or(&serde_json::Map::new())
            .iter()
            .map(|(k, v)| {
                let group_type_index = k.parse().unwrap_or_default();
                let properties: HashMap<String, Value> = v
                    .as_object()
                    .unwrap_or(&serde_json::Map::new())
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                (group_type_index, properties)
            })
            .collect();

        properties_cache.group_properties.extend(group_props_map);
    }

    Ok(())
}

/// Fetch person properties and person ID from the database for a given distinct ID and team ID.
///
/// This function constructs and executes a SQL query to fetch the person properties for a specified distinct ID and team ID.
/// It returns the fetched properties as a HashMap.
async fn fetch_person_properties_from_db(
    reader: PostgresReader,
    distinct_id: String,
    team_id: TeamId,
) -> Result<(HashMap<String, Value>, PersonId), FlagError> {
    let mut conn = reader.as_ref().get_connection().await?;

    let query = r#"
           SELECT "posthog_person"."id" as person_id, "posthog_person"."properties" as person_properties
           FROM "posthog_person"
           INNER JOIN "posthog_persondistinctid" ON ("posthog_person"."id" = "posthog_persondistinctid"."person_id")
           WHERE ("posthog_persondistinctid"."distinct_id" = $1
                   AND "posthog_persondistinctid"."team_id" = $2
                   AND "posthog_person"."team_id" = $2)
           LIMIT 1
       "#;

    let row: Option<(PersonId, Value)> = sqlx::query_as(query)
        .bind(&distinct_id)
        .bind(team_id)
        .fetch_optional(&mut *conn)
        .await?;

    match row {
        Some((person_id, person_props)) => {
            let properties_map = person_props
                .as_object()
                .unwrap_or(&serde_json::Map::new())
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            Ok((properties_map, person_id))
        }
        None => Err(FlagError::PersonNotFound),
    }
}

/// Fetch group properties from the database for a given team ID and group type index.
///
/// This function constructs and executes a SQL query to fetch the group properties for a specified team ID and group type index.
/// It returns the fetched properties as a HashMap.
async fn fetch_group_properties_from_db(
    reader: PostgresReader,
    team_id: TeamId,
    group_type_index: GroupTypeIndex,
    group_key: String,
) -> Result<HashMap<String, Value>, FlagError> {
    let mut conn = reader.as_ref().get_connection().await?;

    let query = r#"
        SELECT "posthog_group"."group_properties"
        FROM "posthog_group"
        WHERE ("posthog_group"."team_id" = $1
                AND "posthog_group"."group_type_index" = $2
                AND "posthog_group"."group_key" = $3)
        LIMIT 1
    "#;

    let row: Option<Value> = sqlx::query_scalar(query)
        .bind(team_id)
        .bind(group_type_index)
        .bind(group_key)
        .fetch_optional(&mut *conn)
        .await?;

    Ok(row
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
        .into_iter()
        .map(|(k, v)| (k, v.clone()))
        .collect())
}

/// Check if all required properties are present in the overrides
/// and none of them are of type "cohort" – if so, return the overrides,
/// otherwise return None, because we can't locally compute cohort properties
fn locally_computable_property_overrides(
    property_overrides: &Option<HashMap<String, Value>>,
    property_filters: &[PropertyFilter],
) -> Option<HashMap<String, Value>> {
    property_overrides.as_ref().and_then(|overrides| {
        let should_prefer_overrides = property_filters
            .iter()
            .all(|prop| overrides.contains_key(&prop.key) && prop.prop_type != "cohort");

        if should_prefer_overrides {
            Some(overrides.clone())
        } else {
            None
        }
    })
}

/// Check if all properties match the given filters
fn all_properties_match(
    flag_condition_properties: &[PropertyFilter],
    matching_property_values: &HashMap<String, Value>,
) -> bool {
    flag_condition_properties
        .iter()
        .all(|property| match_property(property, matching_property_values, false).unwrap_or(false))
}

async fn get_feature_flag_hash_key_overrides(
    reader: PostgresReader,
    team_id: TeamId,
    distinct_id_and_hash_key_override: Vec<String>,
) -> Result<HashMap<String, String>, FlagError> {
    let mut feature_flag_hash_key_overrides = HashMap::new();
    let mut conn = reader.as_ref().get_connection().await?;

    let person_and_distinct_id_query = r#"
            SELECT person_id, distinct_id 
            FROM posthog_persondistinctid 
            WHERE team_id = $1 AND distinct_id = ANY($2)
        "#;

    let person_and_distinct_ids: Vec<(PersonId, String)> =
        sqlx::query_as(person_and_distinct_id_query)
            .bind(team_id)
            .bind(&distinct_id_and_hash_key_override)
            .fetch_all(&mut *conn)
            .await?;

    let person_id_to_distinct_id: HashMap<PersonId, String> =
        person_and_distinct_ids.into_iter().collect();
    let person_ids: Vec<PersonId> = person_id_to_distinct_id.keys().cloned().collect();

    // Get hash key overrides
    let hash_key_override_query = r#"
            SELECT feature_flag_key, hash_key, person_id 
            FROM posthog_featureflaghashkeyoverride 
            WHERE team_id = $1 AND person_id = ANY($2)
        "#;

    let overrides: Vec<(String, String, PersonId)> = sqlx::query_as(hash_key_override_query)
        .bind(team_id)
        .bind(&person_ids)
        .fetch_all(&mut *conn)
        .await?;

    // Sort and process overrides, with the distinct_id at the start of the array having priority
    // We want the highest priority to go last in sort order, so it's the latest update in the hashmap
    let mut sorted_overrides = overrides;
    sorted_overrides.sort_by_key(|(_, _, person_id)| {
        if person_id_to_distinct_id.get(person_id) == Some(&distinct_id_and_hash_key_override[0]) {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Less
        }
    });

    for (feature_flag_key, hash_key, _) in sorted_overrides {
        feature_flag_hash_key_overrides.insert(feature_flag_key, hash_key);
    }

    Ok(feature_flag_hash_key_overrides)
}

async fn set_feature_flag_hash_key_overrides(
    writer: PostgresWriter,
    team_id: TeamId,
    distinct_ids: Vec<String>,
    project_id: ProjectId,
    hash_key_override: String,
) -> Result<bool, FlagError> {
    const MAX_RETRIES: u32 = 2;
    const RETRY_DELAY: Duration = Duration::from_millis(100);

    for retry in 0..MAX_RETRIES {
        let mut conn = writer.get_connection().await?;
        let mut transaction = conn.begin().await?;

        let query = r#"
            WITH target_person_ids AS (
                SELECT team_id, person_id FROM posthog_persondistinctid WHERE team_id = $1 AND
                distinct_id = ANY($2)
            ),
            existing_overrides AS (
                SELECT team_id, person_id, feature_flag_key, hash_key FROM posthog_featureflaghashkeyoverride
                WHERE team_id = $1 AND person_id IN (SELECT person_id FROM target_person_ids)
            ),
            flags_to_override AS (
                SELECT flag.key FROM posthog_featureflag flag
                JOIN posthog_team team ON flag.team_id = team.id
                WHERE team.project_id = $3 
                AND flag.ensure_experience_continuity = TRUE 
                AND flag.active = TRUE 
                AND flag.deleted = FALSE
                AND flag.key NOT IN (SELECT feature_flag_key FROM existing_overrides)
            )
            INSERT INTO posthog_featureflaghashkeyoverride (team_id, person_id, feature_flag_key, hash_key)
                SELECT team_id, person_id, key, $4
                FROM flags_to_override, target_person_ids
                WHERE EXISTS (SELECT 1 FROM posthog_person WHERE id = person_id AND team_id = $1)
            ON CONFLICT DO NOTHING
        "#;

        let result: Result<PgQueryResult, sqlx::Error> = sqlx::query(query)
            .bind(team_id)
            .bind(&distinct_ids)
            .bind(project_id)
            .bind(&hash_key_override)
            .execute(&mut *transaction)
            .await;

        match result {
            Ok(query_result) => {
                // Commit the transaction if successful
                transaction
                    .commit()
                    .await
                    .map_err(|e| FlagError::DatabaseError(e.to_string()))?;
                return Ok(query_result.rows_affected() > 0);
            }
            Err(e) => {
                // Rollback the transaction on error
                transaction
                    .rollback()
                    .await
                    .map_err(|e| FlagError::DatabaseError(e.to_string()))?;

                if e.to_string().contains("violates foreign key constraint")
                    && retry < MAX_RETRIES - 1
                {
                    // Retry logic for specific error
                    tracing::info!(
                        "Retrying set_feature_flag_hash_key_overrides due to person deletion: {:?}",
                        e
                    );
                    sleep(RETRY_DELAY).await;
                } else {
                    return Err(FlagError::DatabaseError(e.to_string()));
                }
            }
        }
    }

    // If we get here, something went wrong
    Ok(false)
}

async fn should_write_hash_key_override(
    reader: PostgresReader,
    team_id: TeamId,
    distinct_id: String,
    project_id: ProjectId,
    hash_key_override: String,
) -> Result<bool, FlagError> {
    const QUERY_TIMEOUT: Duration = Duration::from_millis(1000);
    const MAX_RETRIES: u32 = 2;
    const RETRY_DELAY: Duration = Duration::from_millis(100);

    let distinct_ids = vec![distinct_id, hash_key_override.clone()];

    let query = r#"
        WITH target_person_ids AS (
            SELECT team_id, person_id 
            FROM posthog_persondistinctid 
            WHERE team_id = $1 AND distinct_id = ANY($2)
        ),
        existing_overrides AS (
            SELECT team_id, person_id, feature_flag_key, hash_key 
            FROM posthog_featureflaghashkeyoverride
            WHERE team_id = $1 AND person_id IN (SELECT person_id FROM target_person_ids)
        )
        SELECT key FROM posthog_featureflag flag
        JOIN posthog_team team ON flag.team_id = team.id
        WHERE team.project_id = $3
            AND flag.ensure_experience_continuity = TRUE AND flag.active = TRUE AND flag.deleted = FALSE
            AND key NOT IN (SELECT feature_flag_key FROM existing_overrides)
    "#;

    for retry in 0..MAX_RETRIES {
        let result = timeout(QUERY_TIMEOUT, async {
            let mut conn = reader.get_connection().await.map_err(|e| {
                FlagError::DatabaseError(format!("Failed to acquire connection: {}", e))
            })?;

            let rows = sqlx::query(query)
                .bind(team_id)
                .bind(&distinct_ids)
                .bind(project_id)
                .fetch_all(&mut *conn)
                .await
                .map_err(|e| FlagError::DatabaseError(format!("Query execution failed: {}", e)))?;

            Ok::<bool, FlagError>(!rows.is_empty())
        })
        .await;

        match result {
            Ok(Ok(flags_present)) => return Ok(flags_present),
            Ok(Err(e)) => {
                if e.to_string().contains("violates foreign key constraint")
                    && retry < MAX_RETRIES - 1
                {
                    info!(
                        "Retrying set_feature_flag_hash_key_overrides due to person deletion: {:?}",
                        e
                    );
                    tokio::time::sleep(RETRY_DELAY).await;
                    continue;
                } else {
                    // For other errors or if max retries exceeded, return the error
                    return Err(e);
                }
            }
            Err(_) => {
                // Handle timeout
                return Err(FlagError::TimeoutError);
            }
        }
    }

    // If all retries failed without returning, return false
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use serde_json::json;
    use std::collections::HashMap;

    use crate::{
        flags::flag_models::{
            FeatureFlagRow, FlagFilters, MultivariateFlagOptions, MultivariateFlagVariant,
        },
        properties::property_models::OperatorType,
        utils::test_utils::{
            add_person_to_cohort, get_person_id_by_distinct_id, insert_cohort_for_team_in_pg,
            insert_flag_for_team_in_pg, insert_new_team_in_pg, insert_person_for_team_in_pg,
            setup_pg_reader_client, setup_pg_writer_client,
        },
    };

    #[allow(clippy::too_many_arguments)]
    fn create_test_flag(
        id: Option<i32>,
        team_id: Option<TeamId>,
        name: Option<String>,
        key: Option<String>,
        filters: Option<FlagFilters>,
        deleted: Option<bool>,
        active: Option<bool>,
        ensure_experience_continuity: Option<bool>,
    ) -> FeatureFlag {
        FeatureFlag {
            id: id.unwrap_or(1),
            team_id: team_id.unwrap_or(1),
            name: name.or(Some("Test Flag".to_string())),
            key: key.unwrap_or_else(|| "test_flag".to_string()),
            filters: filters.unwrap_or_else(|| FlagFilters {
                groups: vec![FlagGroupType {
                    properties: Some(vec![]),
                    rollout_percentage: Some(100.0),
                    variant: None,
                }],
                multivariate: None,
                aggregation_group_type_index: None,
                payloads: None,
                super_groups: None,
                holdout_groups: None,
            }),
            deleted: deleted.unwrap_or(false),
            active: active.unwrap_or(true),
            ensure_experience_continuity: ensure_experience_continuity.unwrap_or(false),
            version: Some(1),
        }
    }

    #[tokio::test]
    async fn test_fetch_properties_from_pg_to_match() {
        let reader = setup_pg_reader_client(None).await;
        let writer = setup_pg_writer_client(None).await;
        let cohort_cache = Arc::new(CohortCacheManager::new(reader.clone(), None, None));

        let team = insert_new_team_in_pg(reader.clone(), None)
            .await
            .expect("Failed to insert team in pg");

        let distinct_id = "user_distinct_id".to_string();
        insert_person_for_team_in_pg(reader.clone(), team.id, distinct_id.clone(), None)
            .await
            .expect("Failed to insert person");

        let not_matching_distinct_id = "not_matching_distinct_id".to_string();
        insert_person_for_team_in_pg(
            reader.clone(),
            team.id,
            not_matching_distinct_id.clone(),
            Some(json!({ "email": "a@x.com"})),
        )
        .await
        .expect("Failed to insert person");

        let flag: FeatureFlag = serde_json::from_value(json!(
            {
                "id": 1,
                "team_id": team.id,
                "name": "flag1",
                "key": "flag1",
                "filters": {
                    "groups": [
                        {
                            "properties": [
                                {
                                    "key": "email",
                                    "value": "a@b.com",
                                    "type": "person"
                                }
                            ],
                            "rollout_percentage": 100
                        }
                    ]
                }
            }
        ))
        .unwrap();

        // Matcher for a matching distinct_id
        let mut matcher = FeatureFlagMatcher::new(
            distinct_id.clone(),
            team.id,
            team.project_id,
            reader.clone(),
            writer.clone(),
            cohort_cache.clone(),
            None,
            None,
        );
        let match_result = matcher.get_match(&flag, None, None).await.unwrap();
        assert!(match_result.matches);
        assert_eq!(match_result.variant, None);

        // Matcher for a non-matching distinct_id
        let mut matcher = FeatureFlagMatcher::new(
            not_matching_distinct_id.clone(),
            team.id,
            team.project_id,
            reader.clone(),
            writer.clone(),
            cohort_cache.clone(),
            None,
            None,
        );
        let match_result = matcher.get_match(&flag, None, None).await.unwrap();
        assert!(!match_result.matches);
        assert_eq!(match_result.variant, None);

        // Matcher for a distinct_id that does not exist
        let mut matcher = FeatureFlagMatcher::new(
            "other_distinct_id".to_string(),
            team.id,
            team.project_id,
            reader.clone(),
            writer.clone(),
            cohort_cache.clone(),
            None,
            None,
        );
        let match_result = matcher.get_match(&flag, None, None).await.unwrap();

        // Expecting false for non-existent distinct_id
        assert!(!match_result.matches);
    }

    #[tokio::test]
    async fn test_person_property_overrides() {
        let reader = setup_pg_reader_client(None).await;
        let writer = setup_pg_writer_client(None).await;
        let cohort_cache = Arc::new(CohortCacheManager::new(reader.clone(), None, None));
        let team = insert_new_team_in_pg(reader.clone(), None).await.unwrap();

        let flag = create_test_flag(
            None,
            Some(team.id),
            None,
            None,
            Some(FlagFilters {
                groups: vec![FlagGroupType {
                    properties: Some(vec![PropertyFilter {
                        key: "email".to_string(),
                        value: json!("override@example.com"),
                        operator: None,
                        prop_type: "person".to_string(),
                        group_type_index: None,
                        negation: None,
                    }]),
                    rollout_percentage: Some(100.0),
                    variant: None,
                }],
                multivariate: None,
                aggregation_group_type_index: None,
                payloads: None,
                super_groups: None,
                holdout_groups: None,
            }),
            None,
            None,
            None,
        );

        let overrides = HashMap::from([("email".to_string(), json!("override@example.com"))]);

        let mut matcher = FeatureFlagMatcher::new(
            "test_user".to_string(),
            team.id,
            team.project_id,
            reader,
            writer,
            cohort_cache,
            None,
            None,
        );

        let flags = FeatureFlagList {
            flags: vec![flag.clone()],
        };
        let result = matcher
            .evaluate_all_feature_flags(flags, Some(overrides), None, None)
            .await;
        assert!(!result.errors_while_computing_flags);
        assert_eq!(
            result.flags.get("test_flag").unwrap().to_value(),
            FlagValue::Boolean(true)
        );
    }

    #[tokio::test]
    async fn test_group_property_overrides() {
        let reader = setup_pg_reader_client(None).await;
        let writer = setup_pg_writer_client(None).await;
        let cohort_cache = Arc::new(CohortCacheManager::new(reader.clone(), None, None));
        let team = insert_new_team_in_pg(reader.clone(), None).await.unwrap();

        let flag = create_test_flag(
            None,
            Some(team.id),
            None,
            None,
            Some(FlagFilters {
                groups: vec![FlagGroupType {
                    properties: Some(vec![PropertyFilter {
                        key: "industry".to_string(),
                        value: json!("tech"),
                        operator: None,
                        prop_type: "group".to_string(),
                        group_type_index: Some(1),
                        negation: None,
                    }]),
                    rollout_percentage: Some(100.0),
                    variant: None,
                }],
                multivariate: None,
                aggregation_group_type_index: Some(1),
                payloads: None,
                super_groups: None,
                holdout_groups: None,
            }),
            None,
            None,
            None,
        );

        let mut group_type_mapping_cache =
            GroupTypeMappingCache::new(team.project_id, reader.clone());
        let group_types_to_indexes = [("organization".to_string(), 1)].into_iter().collect();
        group_type_mapping_cache.group_types_to_indexes = group_types_to_indexes;
        group_type_mapping_cache.group_indexes_to_types =
            [(1, "organization".to_string())].into_iter().collect();

        let groups = HashMap::from([("organization".to_string(), json!("org_123"))]);

        let group_overrides = HashMap::from([(
            "organization".to_string(),
            HashMap::from([
                ("industry".to_string(), json!("tech")),
                ("$group_key".to_string(), json!("org_123")),
            ]),
        )]);

        let mut matcher = FeatureFlagMatcher::new(
            "test_user".to_string(),
            team.id,
            team.project_id,
            reader.clone(),
            writer.clone(),
            cohort_cache.clone(),
            Some(group_type_mapping_cache),
            Some(groups),
        );

        let flags = FeatureFlagList {
            flags: vec![flag.clone()],
        };
        let result = matcher
            .evaluate_all_feature_flags(flags, None, Some(group_overrides), None)
            .await;

        let legacy_response = LegacyFlagsResponse::from_response(result);
        assert!(!legacy_response.errors_while_computing_flags);
        assert_eq!(
            legacy_response.feature_flags.get("test_flag"),
            Some(&FlagValue::Boolean(true))
        );
    }

    #[tokio::test]
    async fn test_get_matching_variant_with_cache() {
        let flag = create_test_flag_with_variants(1);
        let reader = setup_pg_reader_client(None).await;
        let writer = setup_pg_writer_client(None).await;
        let cohort_cache = Arc::new(CohortCacheManager::new(reader.clone(), None, None));
        let mut group_type_mapping_cache = GroupTypeMappingCache::new(1, reader.clone());

        let group_types_to_indexes = [("group_type_1".to_string(), 1)].into_iter().collect();
        let group_type_index_to_name = [(1, "group_type_1".to_string())].into_iter().collect();

        group_type_mapping_cache.group_types_to_indexes = group_types_to_indexes;
        group_type_mapping_cache.group_indexes_to_types = group_type_index_to_name;

        let groups = HashMap::from([("group_type_1".to_string(), json!("group_key_1"))]);

        let mut matcher = FeatureFlagMatcher::new(
            "test_user".to_string(),
            1,
            1,
            reader.clone(),
            writer.clone(),
            cohort_cache.clone(),
            Some(group_type_mapping_cache),
            Some(groups),
        );
        let variant = matcher.get_matching_variant(&flag, None).await.unwrap();
        assert!(variant.is_some(), "No variant was selected");
        assert!(
            ["control", "test", "test2"].contains(&variant.unwrap().as_str()),
            "Selected variant is not one of the expected options"
        );
    }

    #[tokio::test]
    async fn test_get_matching_variant_with_db() {
        let reader = setup_pg_reader_client(None).await;
        let writer = setup_pg_writer_client(None).await;
        let cohort_cache = Arc::new(CohortCacheManager::new(reader.clone(), None, None));
        let team = insert_new_team_in_pg(reader.clone(), None).await.unwrap();

        let flag = create_test_flag_with_variants(team.id);

        let mut matcher = FeatureFlagMatcher::new(
            "test_user".to_string(),
            team.id,
            team.project_id,
            reader.clone(),
            writer.clone(),
            cohort_cache.clone(),
            None,
            None,
        );

        let variant = matcher.get_matching_variant(&flag, None).await.unwrap();
        assert!(variant.is_some());
        assert!(["control", "test", "test2"].contains(&variant.unwrap().as_str()));
    }

    #[tokio::test]
    async fn test_is_condition_match_empty_properties() {
        let reader = setup_pg_reader_client(None).await;
        let writer = setup_pg_writer_client(None).await;
        let cohort_cache = Arc::new(CohortCacheManager::new(reader.clone(), None, None));
        let flag = create_test_flag(
            Some(1),
            None,
            None,
            None,
            Some(FlagFilters {
                groups: vec![FlagGroupType {
                    properties: Some(vec![]),
                    rollout_percentage: Some(100.0),
                    variant: None,
                }],
                multivariate: None,
                aggregation_group_type_index: None,
                payloads: None,
                super_groups: None,
                holdout_groups: None,
            }),
            None,
            None,
            None,
        );

        let condition = FlagGroupType {
            variant: None,
            properties: Some(vec![]),
            rollout_percentage: Some(100.0),
        };

        let mut matcher = FeatureFlagMatcher::new(
            "test_user".to_string(),
            1,
            1,
            reader,
            writer,
            cohort_cache,
            None,
            None,
        );
        let (is_match, reason) = matcher
            .is_condition_match(&flag, &condition, None, None)
            .await
            .unwrap();
        assert!(is_match);
        assert_eq!(reason, FeatureFlagMatchReason::ConditionMatch);
    }

    fn create_test_flag_with_variants(team_id: TeamId) -> FeatureFlag {
        FeatureFlag {
            id: 1,
            team_id,
            name: Some("Test Flag".to_string()),
            key: "test_flag".to_string(),
            filters: FlagFilters {
                groups: vec![FlagGroupType {
                    properties: None,
                    rollout_percentage: Some(100.0),
                    variant: None,
                }],
                multivariate: Some(MultivariateFlagOptions {
                    variants: vec![
                        MultivariateFlagVariant {
                            name: Some("Control".to_string()),
                            key: "control".to_string(),
                            rollout_percentage: 33.0,
                        },
                        MultivariateFlagVariant {
                            name: Some("Test".to_string()),
                            key: "test".to_string(),
                            rollout_percentage: 33.0,
                        },
                        MultivariateFlagVariant {
                            name: Some("Test2".to_string()),
                            key: "test2".to_string(),
                            rollout_percentage: 34.0,
                        },
                    ],
                }),
                aggregation_group_type_index: Some(1),
                payloads: None,
                super_groups: None,
                holdout_groups: None,
            },
            deleted: false,
            active: true,
            ensure_experience_continuity: false,
            version: Some(1),
        }
    }

    #[tokio::test]
    async fn test_overrides_avoid_db_lookups() {
        let reader = setup_pg_reader_client(None).await;
        let writer = setup_pg_writer_client(None).await;
        let cohort_cache = Arc::new(CohortCacheManager::new(reader.clone(), None, None));
        let team = insert_new_team_in_pg(reader.clone(), None).await.unwrap();

        let flag = create_test_flag(
            None,
            Some(team.id),
            None,
            None,
            Some(FlagFilters {
                groups: vec![FlagGroupType {
                    properties: Some(vec![PropertyFilter {
                        key: "email".to_string(),
                        value: json!("test@example.com"),
                        operator: Some(OperatorType::Exact),
                        prop_type: "person".to_string(),
                        group_type_index: None,
                        negation: None,
                    }]),
                    rollout_percentage: Some(100.0),
                    variant: None,
                }],
                multivariate: None,
                aggregation_group_type_index: None,
                payloads: None,
                super_groups: None,
                holdout_groups: None,
            }),
            None,
            None,
            None,
        );

        let person_property_overrides =
            HashMap::from([("email".to_string(), json!("test@example.com"))]);

        let mut matcher = FeatureFlagMatcher::new(
            "test_user".to_string(),
            team.id,
            team.project_id,
            reader.clone(),
            writer.clone(),
            cohort_cache.clone(),
            None,
            None,
        );

        let result = matcher
            .evaluate_all_feature_flags(
                FeatureFlagList {
                    flags: vec![flag.clone()],
                },
                Some(person_property_overrides),
                None,
                None,
            )
            .await;

        let legacy_response = LegacyFlagsResponse::from_response(result);
        assert!(!legacy_response.errors_while_computing_flags);
        assert_eq!(
            legacy_response.feature_flags.get("test_flag"),
            Some(&FlagValue::Boolean(true))
        );

        let cache = &matcher.flag_evaluation_state;
        assert!(cache.person_properties.is_none());
    }

    #[tokio::test]
    async fn test_fallback_to_db_when_overrides_insufficient() {
        let reader = setup_pg_reader_client(None).await;
        let writer = setup_pg_writer_client(None).await;
        let cohort_cache = Arc::new(CohortCacheManager::new(reader.clone(), None, None));
        let team = insert_new_team_in_pg(reader.clone(), None).await.unwrap();

        let flag = create_test_flag(
            None,
            Some(team.id),
            None,
            None,
            Some(FlagFilters {
                groups: vec![FlagGroupType {
                    properties: Some(vec![
                        PropertyFilter {
                            key: "email".to_string(),
                            value: json!("test@example.com"),
                            operator: Some(OperatorType::Exact),
                            prop_type: "person".to_string(),
                            group_type_index: None,
                            negation: None,
                        },
                        PropertyFilter {
                            key: "age".to_string(),
                            value: json!(25),
                            operator: Some(OperatorType::Gte),
                            prop_type: "person".to_string(),
                            group_type_index: None,
                            negation: None,
                        },
                    ]),
                    rollout_percentage: Some(100.0),
                    variant: None,
                }],
                multivariate: None,
                aggregation_group_type_index: None,
                payloads: None,
                super_groups: None,
                holdout_groups: None,
            }),
            None,
            None,
            None,
        );

        let person_property_overrides = Some(HashMap::from([(
            "email".to_string(),
            json!("test@example.com"),
        )]));

        insert_person_for_team_in_pg(
            reader.clone(),
            team.id,
            "test_user".to_string(),
            Some(json!({"email": "test@example.com", "age": 30})),
        )
        .await
        .unwrap();

        let mut matcher = FeatureFlagMatcher::new(
            "test_user".to_string(),
            team.id,
            team.project_id,
            reader.clone(),
            writer.clone(),
            cohort_cache.clone(),
            None,
            None,
        );

        let result = matcher
            .get_match(&flag, person_property_overrides.clone(), None)
            .await
            .unwrap();

        assert!(result.matches);

        let cache = &matcher.flag_evaluation_state;
        assert!(cache.person_properties.is_some());
        assert_eq!(
            cache.person_properties.as_ref().unwrap().get("age"),
            Some(&json!(30))
        );
    }

    #[tokio::test]
    async fn test_property_fetching_and_caching() {
        let reader = setup_pg_reader_client(None).await;
        let writer = setup_pg_writer_client(None).await;
        let cohort_cache = Arc::new(CohortCacheManager::new(reader.clone(), None, None));
        let team = insert_new_team_in_pg(reader.clone(), None).await.unwrap();

        let distinct_id = "test_user".to_string();
        insert_person_for_team_in_pg(
            reader.clone(),
            team.id,
            distinct_id.clone(),
            Some(json!({"email": "test@example.com", "age": 30})),
        )
        .await
        .unwrap();

        let mut matcher = FeatureFlagMatcher::new(
            distinct_id,
            team.id,
            team.project_id,
            reader.clone(),
            writer.clone(),
            cohort_cache.clone(),
            None,
            None,
        );

        let properties = matcher
            .get_person_properties_from_cache_or_db()
            .await
            .unwrap();

        assert_eq!(properties.get("email").unwrap(), &json!("test@example.com"));
        assert_eq!(properties.get("age").unwrap(), &json!(30));

        let cached_properties = matcher.flag_evaluation_state.person_properties.clone();
        assert!(cached_properties.is_some());
        assert_eq!(
            cached_properties.unwrap().get("email").unwrap(),
            &json!("test@example.com")
        );
    }

    #[tokio::test]
    async fn test_property_caching() {
        let reader = setup_pg_reader_client(None).await;
        let writer = setup_pg_writer_client(None).await;
        let cohort_cache = Arc::new(CohortCacheManager::new(reader.clone(), None, None));
        let team = insert_new_team_in_pg(reader.clone(), None).await.unwrap();

        let distinct_id = "test_user".to_string();
        insert_person_for_team_in_pg(
            reader.clone(),
            team.id,
            distinct_id.clone(),
            Some(json!({"email": "test@example.com", "age": 30})),
        )
        .await
        .unwrap();

        let mut matcher = FeatureFlagMatcher::new(
            distinct_id.clone(),
            team.id,
            team.project_id,
            reader.clone(),
            writer.clone(),
            cohort_cache.clone(),
            None,
            None,
        );

        // First access should fetch from the database
        let start = std::time::Instant::now();
        let properties = matcher
            .get_person_properties_from_cache_or_db()
            .await
            .unwrap();
        let first_duration = start.elapsed();

        // Second access should use the cache and be faster
        let start = std::time::Instant::now();
        let cached_properties = matcher
            .get_person_properties_from_cache_or_db()
            .await
            .unwrap();
        let second_duration = start.elapsed();

        assert_eq!(properties, cached_properties);
        assert!(
            second_duration < first_duration,
            "Second access should be faster due to caching"
        );

        // Create a new matcher to simulate a fresh state
        let mut new_matcher = FeatureFlagMatcher::new(
            distinct_id.clone(),
            team.id,
            team.project_id,
            reader.clone(),
            writer.clone(),
            cohort_cache.clone(),
            None,
            None,
        );

        // First access with new matcher should fetch from the database again
        let start = std::time::Instant::now();
        let new_properties = new_matcher
            .get_person_properties_from_cache_or_db()
            .await
            .unwrap();
        let new_first_duration = start.elapsed();

        assert_eq!(properties, new_properties);
        assert!(
            new_first_duration > second_duration,
            "First access with new matcher should be slower than cached access"
        );

        // Second access with new matcher should use the cache and be faster
        let start = std::time::Instant::now();
        let new_cached_properties = new_matcher
            .get_person_properties_from_cache_or_db()
            .await
            .unwrap();
        let new_second_duration = start.elapsed();

        assert_eq!(properties, new_cached_properties);
        assert!(
            new_second_duration < new_first_duration,
            "Second access with new matcher should be faster due to caching"
        );
    }

    #[tokio::test]
    async fn test_overrides_locally_computable() {
        let overrides = Some(HashMap::from([
            ("email".to_string(), json!("test@example.com")),
            ("age".to_string(), json!(30)),
        ]));

        let property_filters = vec![
            PropertyFilter {
                key: "email".to_string(),
                value: json!("test@example.com"),
                operator: None,
                prop_type: "person".to_string(),
                group_type_index: None,
                negation: None,
            },
            PropertyFilter {
                key: "age".to_string(),
                value: json!(25),
                operator: Some(OperatorType::Gte),
                prop_type: "person".to_string(),
                group_type_index: None,
                negation: None,
            },
        ];

        let result = locally_computable_property_overrides(&overrides, &property_filters);
        assert!(result.is_some());

        let property_filters_with_cohort = vec![
            PropertyFilter {
                key: "email".to_string(),
                value: json!("test@example.com"),
                operator: None,
                prop_type: "person".to_string(),
                group_type_index: None,
                negation: None,
            },
            PropertyFilter {
                key: "cohort".to_string(),
                value: json!(1),
                operator: None,
                prop_type: "cohort".to_string(),
                group_type_index: None,
                negation: None,
            },
        ];

        let result =
            locally_computable_property_overrides(&overrides, &property_filters_with_cohort);
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_concurrent_flag_evaluation() {
        let reader = setup_pg_reader_client(None).await;
        let writer = setup_pg_writer_client(None).await;
        let cohort_cache = Arc::new(CohortCacheManager::new(reader.clone(), None, None));
        let team = insert_new_team_in_pg(reader.clone(), None).await.unwrap();
        let flag = Arc::new(create_test_flag(
            None,
            Some(team.id),
            None,
            None,
            Some(FlagFilters {
                groups: vec![FlagGroupType {
                    properties: Some(vec![]),
                    rollout_percentage: Some(100.0),
                    variant: None,
                }],
                multivariate: None,
                aggregation_group_type_index: None,
                payloads: None,
                super_groups: None,
                holdout_groups: None,
            }),
            None,
            None,
            None,
        ));

        let mut handles = vec![];
        for i in 0..100 {
            let flag_clone = flag.clone();
            let reader_clone = reader.clone();
            let writer_clone = writer.clone();
            let cohort_cache_clone = cohort_cache.clone();
            handles.push(tokio::spawn(async move {
                let mut matcher = FeatureFlagMatcher::new(
                    format!("test_user_{}", i),
                    team.id,
                    team.project_id,
                    reader_clone,
                    writer_clone,
                    cohort_cache_clone,
                    None,
                    None,
                );
                matcher.get_match(&flag_clone, None, None).await.unwrap()
            }));
        }

        let results: Vec<FeatureFlagMatch> = futures::future::join_all(handles)
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();

        // Check that all evaluations completed without errors
        assert_eq!(results.len(), 100);
    }

    #[tokio::test]
    async fn test_property_operators() {
        let reader = setup_pg_reader_client(None).await;
        let writer = setup_pg_writer_client(None).await;
        let cohort_cache = Arc::new(CohortCacheManager::new(reader.clone(), None, None));
        let team = insert_new_team_in_pg(reader.clone(), None).await.unwrap();

        let flag = create_test_flag(
            None,
            Some(team.id),
            None,
            None,
            Some(FlagFilters {
                groups: vec![FlagGroupType {
                    properties: Some(vec![
                        PropertyFilter {
                            key: "age".to_string(),
                            value: json!(25),
                            operator: Some(OperatorType::Gte),
                            prop_type: "person".to_string(),
                            group_type_index: None,
                            negation: None,
                        },
                        PropertyFilter {
                            key: "email".to_string(),
                            value: json!("example@domain.com"),
                            operator: Some(OperatorType::Icontains),
                            prop_type: "person".to_string(),
                            group_type_index: None,
                            negation: None,
                        },
                    ]),
                    rollout_percentage: Some(100.0),
                    variant: None,
                }],
                multivariate: None,
                aggregation_group_type_index: None,
                payloads: None,
                super_groups: None,
                holdout_groups: None,
            }),
            None,
            None,
            None,
        );

        insert_person_for_team_in_pg(
            reader.clone(),
            team.id,
            "test_user".to_string(),
            Some(json!({"email": "user@example@domain.com", "age": 30})),
        )
        .await
        .unwrap();

        let mut matcher = FeatureFlagMatcher::new(
            "test_user".to_string(),
            team.id,
            team.project_id,
            reader.clone(),
            writer.clone(),
            cohort_cache.clone(),
            None,
            None,
        );

        let result = matcher.get_match(&flag, None, None).await.unwrap();

        assert!(result.matches);
    }

    #[tokio::test]
    async fn test_empty_hashed_identifier() {
        let reader = setup_pg_reader_client(None).await;
        let writer = setup_pg_writer_client(None).await;
        let cohort_cache = Arc::new(CohortCacheManager::new(reader.clone(), None, None));
        let flag = create_test_flag(
            Some(1),
            None,
            None,
            None,
            Some(FlagFilters {
                groups: vec![FlagGroupType {
                    properties: Some(vec![]),
                    rollout_percentage: Some(100.0),
                    variant: None,
                }],
                multivariate: None,
                aggregation_group_type_index: None,
                payloads: None,
                super_groups: None,
                holdout_groups: None,
            }),
            None,
            None,
            None,
        );

        let mut matcher = FeatureFlagMatcher::new(
            "".to_string(),
            1,
            1,
            reader,
            writer,
            cohort_cache,
            None,
            None,
        );

        let result = matcher.get_match(&flag, None, None).await.unwrap();

        assert!(!result.matches);
    }

    #[tokio::test]
    async fn test_rollout_percentage() {
        let reader = setup_pg_reader_client(None).await;
        let writer = setup_pg_writer_client(None).await;
        let cohort_cache = Arc::new(CohortCacheManager::new(reader.clone(), None, None));
        let mut flag = create_test_flag(
            Some(1),
            None,
            None,
            None,
            Some(FlagFilters {
                groups: vec![FlagGroupType {
                    properties: Some(vec![]),
                    rollout_percentage: Some(0.0),
                    variant: None,
                }],
                multivariate: None,
                aggregation_group_type_index: None,
                payloads: None,
                super_groups: None,
                holdout_groups: None,
            }),
            None,
            None,
            None,
        );

        let mut matcher = FeatureFlagMatcher::new(
            "test_user".to_string(),
            1,
            1,
            reader,
            writer,
            cohort_cache,
            None,
            None,
        );

        let result = matcher.get_match(&flag, None, None).await.unwrap();

        assert!(!result.matches);

        // Now set the rollout percentage to 100%
        flag.filters.groups[0].rollout_percentage = Some(100.0);

        let result = matcher.get_match(&flag, None, None).await.unwrap();

        assert!(result.matches);
    }

    #[tokio::test]
    async fn test_uneven_variant_distribution() {
        let reader = setup_pg_reader_client(None).await;
        let writer = setup_pg_writer_client(None).await;
        let cohort_cache = Arc::new(CohortCacheManager::new(reader.clone(), None, None));
        let mut flag = create_test_flag_with_variants(1);

        // Adjust variant rollout percentages to be uneven
        flag.filters.multivariate.as_mut().unwrap().variants = vec![
            MultivariateFlagVariant {
                name: Some("Control".to_string()),
                key: "control".to_string(),
                rollout_percentage: 10.0,
            },
            MultivariateFlagVariant {
                name: Some("Test".to_string()),
                key: "test".to_string(),
                rollout_percentage: 30.0,
            },
            MultivariateFlagVariant {
                name: Some("Test2".to_string()),
                key: "test2".to_string(),
                rollout_percentage: 60.0,
            },
        ];

        // Ensure the flag is person-based by setting aggregation_group_type_index to None
        flag.filters.aggregation_group_type_index = None;

        let mut matcher = FeatureFlagMatcher::new(
            "test_user".to_string(),
            1,
            1,
            reader,
            writer,
            cohort_cache,
            None,
            None,
        );

        let mut control_count = 0;
        let mut test_count = 0;
        let mut test2_count = 0;

        // Run the test multiple times to simulate distribution
        for i in 0..1000 {
            matcher.distinct_id = format!("user_{}", i);
            let variant = matcher.get_matching_variant(&flag, None).await.unwrap();
            match variant.as_deref() {
                Some("control") => control_count += 1,
                Some("test") => test_count += 1,
                Some("test2") => test2_count += 1,
                _ => (),
            }
        }

        // Check that the distribution roughly matches the rollout percentages
        let total = control_count + test_count + test2_count;
        assert!((control_count as f64 / total as f64 - 0.10).abs() < 0.05);
        assert!((test_count as f64 / total as f64 - 0.30).abs() < 0.05);
        assert!((test2_count as f64 / total as f64 - 0.60).abs() < 0.05);
    }

    #[tokio::test]
    async fn test_missing_properties_in_db() {
        let reader = setup_pg_reader_client(None).await;
        let writer = setup_pg_writer_client(None).await;
        let cohort_cache = Arc::new(CohortCacheManager::new(reader.clone(), None, None));
        let team = insert_new_team_in_pg(reader.clone(), None).await.unwrap();

        // Insert a person without properties
        insert_person_for_team_in_pg(reader.clone(), team.id, "test_user".to_string(), None)
            .await
            .unwrap();

        let flag = create_test_flag(
            None,
            Some(team.id),
            None,
            None,
            Some(FlagFilters {
                groups: vec![FlagGroupType {
                    properties: Some(vec![PropertyFilter {
                        key: "email".to_string(),
                        value: json!("test@example.com"),
                        operator: None,
                        prop_type: "person".to_string(),
                        group_type_index: None,
                        negation: None,
                    }]),
                    rollout_percentage: Some(100.0),
                    variant: None,
                }],
                multivariate: None,
                aggregation_group_type_index: None,
                payloads: None,
                super_groups: None,
                holdout_groups: None,
            }),
            None,
            None,
            None,
        );

        let mut matcher = FeatureFlagMatcher::new(
            "test_user".to_string(),
            team.id,
            team.project_id,
            reader.clone(),
            writer.clone(),
            cohort_cache,
            None,
            None,
        );

        let result = matcher.get_match(&flag, None, None).await.unwrap();

        assert!(!result.matches);
    }

    #[tokio::test]
    async fn test_malformed_property_data() {
        let reader = setup_pg_reader_client(None).await;
        let writer = setup_pg_writer_client(None).await;
        let cohort_cache = Arc::new(CohortCacheManager::new(reader.clone(), None, None));
        let team = insert_new_team_in_pg(reader.clone(), None).await.unwrap();

        // Insert a person with malformed properties
        insert_person_for_team_in_pg(
            reader.clone(),
            team.id,
            "test_user".to_string(),
            Some(json!({"age": "not_a_number"})),
        )
        .await
        .unwrap();

        let flag = create_test_flag(
            None,
            Some(team.id),
            None,
            None,
            Some(FlagFilters {
                groups: vec![FlagGroupType {
                    properties: Some(vec![PropertyFilter {
                        key: "age".to_string(),
                        value: json!(25),
                        operator: Some(OperatorType::Gte),
                        prop_type: "person".to_string(),
                        group_type_index: None,
                        negation: None,
                    }]),
                    rollout_percentage: Some(100.0),
                    variant: None,
                }],
                multivariate: None,
                aggregation_group_type_index: None,
                payloads: None,
                super_groups: None,
                holdout_groups: None,
            }),
            None,
            None,
            None,
        );

        let mut matcher = FeatureFlagMatcher::new(
            "test_user".to_string(),
            team.id,
            team.project_id,
            reader.clone(),
            writer.clone(),
            cohort_cache,
            None,
            None,
        );

        let result = matcher.get_match(&flag, None, None).await.unwrap();

        // The match should fail due to invalid data type
        assert!(!result.matches);
    }

    #[tokio::test]
    async fn test_get_match_with_insufficient_overrides() {
        let reader = setup_pg_reader_client(None).await;
        let writer = setup_pg_writer_client(None).await;
        let cohort_cache = Arc::new(CohortCacheManager::new(reader.clone(), None, None));
        let team = insert_new_team_in_pg(reader.clone(), None).await.unwrap();

        let flag = create_test_flag(
            None,
            Some(team.id),
            None,
            None,
            Some(FlagFilters {
                groups: vec![FlagGroupType {
                    properties: Some(vec![
                        PropertyFilter {
                            key: "email".to_string(),
                            value: json!("test@example.com"),
                            operator: None,
                            prop_type: "person".to_string(),
                            group_type_index: None,
                            negation: None,
                        },
                        PropertyFilter {
                            key: "age".to_string(),
                            value: json!(25),
                            operator: Some(OperatorType::Gte),
                            prop_type: "person".to_string(),
                            group_type_index: None,
                            negation: None,
                        },
                    ]),
                    rollout_percentage: Some(100.0),
                    variant: None,
                }],
                multivariate: None,
                aggregation_group_type_index: None,
                payloads: None,
                super_groups: None,
                holdout_groups: None,
            }),
            None,
            None,
            None,
        );

        let person_overrides = Some(HashMap::from([(
            "email".to_string(),
            json!("test@example.com"),
        )]));

        insert_person_for_team_in_pg(
            reader.clone(),
            team.id,
            "test_user".to_string(),
            Some(json!({"email": "test@example.com", "age": 30})),
        )
        .await
        .unwrap();

        let mut matcher = FeatureFlagMatcher::new(
            "test_user".to_string(),
            team.id,
            team.project_id,
            reader.clone(),
            writer.clone(),
            cohort_cache,
            None,
            None,
        );

        let result = matcher
            .get_match(&flag, person_overrides, None)
            .await
            .unwrap();

        assert!(result.matches);
    }

    #[tokio::test]
    async fn test_evaluation_reasons() {
        let reader = setup_pg_reader_client(None).await;
        let writer = setup_pg_writer_client(None).await;
        let cohort_cache = Arc::new(CohortCacheManager::new(reader.clone(), None, None));
        let flag = create_test_flag(
            Some(1),
            None,
            None,
            None,
            Some(FlagFilters {
                groups: vec![FlagGroupType {
                    properties: Some(vec![]),
                    rollout_percentage: Some(100.0),
                    variant: None,
                }],
                multivariate: None,
                aggregation_group_type_index: None,
                payloads: None,
                super_groups: None,
                holdout_groups: None,
            }),
            None,
            None,
            None,
        );

        let mut matcher = FeatureFlagMatcher::new(
            "test_user".to_string(),
            1,
            1,
            reader.clone(),
            writer.clone(),
            cohort_cache,
            None,
            None,
        );

        let (is_match, reason) = matcher
            .is_condition_match(&flag, &flag.filters.groups[0], None, None)
            .await
            .unwrap();

        assert!(is_match);
        assert_eq!(reason, FeatureFlagMatchReason::ConditionMatch);
    }

    #[tokio::test]
    async fn test_complex_conditions() {
        let reader = setup_pg_reader_client(None).await;
        let writer = setup_pg_writer_client(None).await;
        let cohort_cache = Arc::new(CohortCacheManager::new(reader.clone(), None, None));
        let team = insert_new_team_in_pg(reader.clone(), None).await.unwrap();

        let flag = create_test_flag(
            Some(1),
            Some(team.id),
            Some("Complex Flag".to_string()),
            Some("complex_flag".to_string()),
            Some(FlagFilters {
                groups: vec![
                    FlagGroupType {
                        properties: Some(vec![PropertyFilter {
                            key: "email".to_string(),
                            value: json!("user1@example.com"),
                            operator: None,
                            prop_type: "person".to_string(),
                            group_type_index: None,
                            negation: None,
                        }]),
                        rollout_percentage: Some(100.0),
                        variant: None,
                    },
                    FlagGroupType {
                        properties: Some(vec![PropertyFilter {
                            key: "age".to_string(),
                            value: json!(30),
                            operator: Some(OperatorType::Gte),
                            prop_type: "person".to_string(),
                            group_type_index: None,
                            negation: None,
                        }]),
                        rollout_percentage: Some(100.0),
                        variant: None,
                    },
                ],
                multivariate: None,
                aggregation_group_type_index: None,
                payloads: None,
                super_groups: None,
                holdout_groups: None,
            }),
            Some(false),
            Some(true),
            Some(false),
        );

        insert_person_for_team_in_pg(
            reader.clone(),
            team.id,
            "test_user".to_string(),
            Some(json!({"email": "user2@example.com", "age": 35})),
        )
        .await
        .unwrap();

        let mut matcher = FeatureFlagMatcher::new(
            "test_user".to_string(),
            team.id,
            team.project_id,
            reader.clone(),
            writer.clone(),
            cohort_cache,
            None,
            None,
        );

        let result = matcher.get_match(&flag, None, None).await.unwrap();

        assert!(result.matches);
    }

    #[tokio::test]
    async fn test_super_condition_matches_boolean() {
        let reader = setup_pg_reader_client(None).await;
        let writer = setup_pg_writer_client(None).await;
        let cohort_cache = Arc::new(CohortCacheManager::new(reader.clone(), None, None));
        let team = insert_new_team_in_pg(reader.clone(), None).await.unwrap();

        let flag = create_test_flag(
            Some(1),
            Some(team.id),
            Some("Super Condition Flag".to_string()),
            Some("super_condition_flag".to_string()),
            Some(FlagFilters {
                groups: vec![
                    FlagGroupType {
                        properties: Some(vec![PropertyFilter {
                            key: "email".to_string(),
                            value: json!("fake@posthog.com"),
                            operator: Some(OperatorType::Exact),
                            prop_type: "person".to_string(),
                            group_type_index: None,
                            negation: None,
                        }]),
                        rollout_percentage: Some(0.0),
                        variant: None,
                    },
                    FlagGroupType {
                        properties: Some(vec![PropertyFilter {
                            key: "email".to_string(),
                            value: json!("test@posthog.com"),
                            operator: Some(OperatorType::Exact),
                            prop_type: "person".to_string(),
                            group_type_index: None,
                            negation: None,
                        }]),
                        rollout_percentage: Some(100.0),
                        variant: None,
                    },
                    FlagGroupType {
                        properties: None,
                        rollout_percentage: Some(50.0),
                        variant: None,
                    },
                ],
                multivariate: None,
                aggregation_group_type_index: None,
                payloads: None,
                super_groups: Some(vec![FlagGroupType {
                    properties: Some(vec![PropertyFilter {
                        key: "is_enabled".to_string(),
                        value: json!(["true"]),
                        operator: Some(OperatorType::Exact),
                        prop_type: "person".to_string(),
                        group_type_index: None,
                        negation: None,
                    }]),
                    rollout_percentage: Some(100.0),
                    variant: None,
                }]),
                holdout_groups: None,
            }),
            None,
            None,
            None,
        );

        insert_person_for_team_in_pg(
            reader.clone(),
            team.id,
            "test_id".to_string(),
            Some(json!({"email": "test@posthog.com", "is_enabled": true})),
        )
        .await
        .unwrap();

        insert_person_for_team_in_pg(reader.clone(), team.id, "lil_id".to_string(), None)
            .await
            .unwrap();

        insert_person_for_team_in_pg(reader.clone(), team.id, "another_id".to_string(), None)
            .await
            .unwrap();

        let mut matcher_test_id = FeatureFlagMatcher::new(
            "test_id".to_string(),
            team.id,
            team.project_id,
            reader.clone(),
            writer.clone(),
            cohort_cache.clone(),
            None,
            None,
        );

        let mut matcher_example_id = FeatureFlagMatcher::new(
            "lil_id".to_string(),
            team.id,
            team.project_id,
            reader.clone(),
            writer.clone(),
            cohort_cache.clone(),
            None,
            None,
        );

        let mut matcher_another_id = FeatureFlagMatcher::new(
            "another_id".to_string(),
            team.id,
            team.project_id,
            reader.clone(),
            writer.clone(),
            cohort_cache.clone(),
            None,
            None,
        );

        let result_test_id = matcher_test_id.get_match(&flag, None, None).await.unwrap();
        let result_example_id = matcher_example_id
            .get_match(&flag, None, None)
            .await
            .unwrap();
        let result_another_id = matcher_another_id
            .get_match(&flag, None, None)
            .await
            .unwrap();

        assert!(result_test_id.matches);
        assert!(result_test_id.reason == FeatureFlagMatchReason::SuperConditionValue);
        assert!(result_example_id.matches);
        assert!(result_example_id.reason == FeatureFlagMatchReason::ConditionMatch);
        assert!(!result_another_id.matches);
        assert!(result_another_id.reason == FeatureFlagMatchReason::OutOfRolloutBound);
    }

    #[tokio::test]
    async fn test_super_condition_matches_string() {
        let reader = setup_pg_reader_client(None).await;
        let writer = setup_pg_writer_client(None).await;
        let cohort_cache = Arc::new(CohortCacheManager::new(reader.clone(), None, None));
        let team = insert_new_team_in_pg(reader.clone(), None).await.unwrap();

        insert_person_for_team_in_pg(
            reader.clone(),
            team.id,
            "test_id".to_string(),
            Some(json!({"email": "test@posthog.com", "is_enabled": "true"})),
        )
        .await
        .unwrap();

        let flag = create_test_flag(
            Some(1),
            Some(team.id),
            Some("Super Condition Flag".to_string()),
            Some("super_condition_flag".to_string()),
            Some(FlagFilters {
                groups: vec![
                    FlagGroupType {
                        properties: Some(vec![PropertyFilter {
                            key: "email".to_string(),
                            value: json!("fake@posthog.com"),
                            operator: Some(OperatorType::Exact),
                            prop_type: "person".to_string(),
                            group_type_index: None,
                            negation: None,
                        }]),
                        rollout_percentage: Some(0.0),
                        variant: None,
                    },
                    FlagGroupType {
                        properties: Some(vec![PropertyFilter {
                            key: "email".to_string(),
                            value: json!("test@posthog.com"),
                            operator: Some(OperatorType::Exact),
                            prop_type: "person".to_string(),
                            group_type_index: None,
                            negation: None,
                        }]),
                        rollout_percentage: Some(100.0),
                        variant: None,
                    },
                    FlagGroupType {
                        properties: None,
                        rollout_percentage: Some(50.0),
                        variant: None,
                    },
                ],
                multivariate: None,
                aggregation_group_type_index: None,
                payloads: None,
                super_groups: Some(vec![FlagGroupType {
                    properties: Some(vec![PropertyFilter {
                        key: "is_enabled".to_string(),
                        value: json!("true"),
                        operator: Some(OperatorType::Exact),
                        prop_type: "person".to_string(),
                        group_type_index: None,
                        negation: None,
                    }]),
                    rollout_percentage: Some(100.0),
                    variant: None,
                }]),
                holdout_groups: None,
            }),
            None,
            None,
            None,
        );

        let mut matcher = FeatureFlagMatcher::new(
            "test_id".to_string(),
            team.id,
            team.project_id,
            reader.clone(),
            writer.clone(),
            cohort_cache.clone(),
            None,
            None,
        );

        let result = matcher.get_match(&flag, None, None).await.unwrap();

        assert!(result.matches);
        assert_eq!(result.reason, FeatureFlagMatchReason::SuperConditionValue);
        assert_eq!(result.condition_index, Some(0));
    }

    #[tokio::test]
    async fn test_super_condition_matches_and_false() {
        let reader = setup_pg_reader_client(None).await;
        let writer = setup_pg_writer_client(None).await;
        let cohort_cache = Arc::new(CohortCacheManager::new(reader.clone(), None, None));
        let team = insert_new_team_in_pg(reader.clone(), None).await.unwrap();

        insert_person_for_team_in_pg(
            reader.clone(),
            team.id,
            "test_id".to_string(),
            Some(json!({"email": "test@posthog.com", "is_enabled": true})),
        )
        .await
        .unwrap();

        insert_person_for_team_in_pg(reader.clone(), team.id, "another_id".to_string(), None)
            .await
            .unwrap();

        insert_person_for_team_in_pg(reader.clone(), team.id, "lil_id".to_string(), None)
            .await
            .unwrap();

        let flag = create_test_flag(
            Some(1),
            Some(team.id),
            Some("Super Condition Flag".to_string()),
            Some("super_condition_flag".to_string()),
            Some(FlagFilters {
                groups: vec![
                    FlagGroupType {
                        properties: Some(vec![PropertyFilter {
                            key: "email".to_string(),
                            value: json!("fake@posthog.com"),
                            operator: Some(OperatorType::Exact),
                            prop_type: "person".to_string(),
                            group_type_index: None,
                            negation: None,
                        }]),
                        rollout_percentage: Some(0.0),
                        variant: None,
                    },
                    FlagGroupType {
                        properties: Some(vec![PropertyFilter {
                            key: "email".to_string(),
                            value: json!("test@posthog.com"),
                            operator: Some(OperatorType::Exact),
                            prop_type: "person".to_string(),
                            group_type_index: None,
                            negation: None,
                        }]),
                        rollout_percentage: Some(100.0),
                        variant: None,
                    },
                    FlagGroupType {
                        properties: None,
                        rollout_percentage: Some(50.0),
                        variant: None,
                    },
                ],
                multivariate: None,
                aggregation_group_type_index: None,
                payloads: None,
                super_groups: Some(vec![FlagGroupType {
                    properties: Some(vec![PropertyFilter {
                        key: "is_enabled".to_string(),
                        value: json!(false),
                        operator: Some(OperatorType::Exact),
                        prop_type: "person".to_string(),
                        group_type_index: None,
                        negation: None,
                    }]),
                    rollout_percentage: Some(100.0),
                    variant: None,
                }]),
                holdout_groups: None,
            }),
            None,
            None,
            None,
        );

        let mut matcher_test_id = FeatureFlagMatcher::new(
            "test_id".to_string(),
            team.id,
            team.project_id,
            reader.clone(),
            writer.clone(),
            cohort_cache.clone(),
            None,
            None,
        );

        let mut matcher_example_id = FeatureFlagMatcher::new(
            "lil_id".to_string(),
            team.id,
            team.project_id,
            reader.clone(),
            writer.clone(),
            cohort_cache.clone(),
            None,
            None,
        );

        let mut matcher_another_id = FeatureFlagMatcher::new(
            "another_id".to_string(),
            team.id,
            team.project_id,
            reader.clone(),
            writer.clone(),
            cohort_cache.clone(),
            None,
            None,
        );

        let result_test_id = matcher_test_id.get_match(&flag, None, None).await.unwrap();
        let result_example_id = matcher_example_id
            .get_match(&flag, None, None)
            .await
            .unwrap();
        let result_another_id = matcher_another_id
            .get_match(&flag, None, None)
            .await
            .unwrap();

        assert!(!result_test_id.matches);
        assert_eq!(
            result_test_id.reason,
            FeatureFlagMatchReason::SuperConditionValue
        );
        assert_eq!(result_test_id.condition_index, Some(0));

        assert!(result_example_id.matches);
        assert_eq!(
            result_example_id.reason,
            FeatureFlagMatchReason::ConditionMatch
        );
        assert_eq!(result_example_id.condition_index, Some(2));

        assert!(!result_another_id.matches);
        assert_eq!(
            result_another_id.reason,
            FeatureFlagMatchReason::OutOfRolloutBound
        );
        assert_eq!(result_another_id.condition_index, Some(2));
    }

    #[tokio::test]
    async fn test_basic_cohort_matching() {
        let reader = setup_pg_reader_client(None).await;
        let writer = setup_pg_writer_client(None).await;
        let cohort_cache = Arc::new(CohortCacheManager::new(reader.clone(), None, None));
        let team = insert_new_team_in_pg(reader.clone(), None).await.unwrap();

        // Insert a cohort with the condition that matches the test user's properties
        let cohort_row = insert_cohort_for_team_in_pg(
            reader.clone(),
            team.id,
            None,
            json!({
                "properties": {
                    "type": "OR",
                    "values": [{
                        "type": "OR",
                        "values": [{
                            "key": "$browser_version",
                            "type": "person",
                            "value": "125",
                            "negation": false,
                            "operator": "gt"
                        }]
                    }]
                }
            }),
            false,
        )
        .await
        .unwrap();

        // Insert a person with properties that match the cohort condition
        insert_person_for_team_in_pg(
            reader.clone(),
            team.id,
            "test_user".to_string(),
            Some(json!({"$browser_version": 126})),
        )
        .await
        .unwrap();

        // Define a flag with a cohort filter
        let flag = create_test_flag(
            None,
            Some(team.id),
            None,
            None,
            Some(FlagFilters {
                groups: vec![FlagGroupType {
                    properties: Some(vec![PropertyFilter {
                        key: "id".to_string(),
                        value: json!(cohort_row.id),
                        operator: Some(OperatorType::In),
                        prop_type: "cohort".to_string(),
                        group_type_index: None,
                        negation: Some(false),
                    }]),
                    rollout_percentage: Some(100.0),
                    variant: None,
                }],
                multivariate: None,
                aggregation_group_type_index: None,
                payloads: None,
                super_groups: None,
                holdout_groups: None,
            }),
            None,
            None,
            None,
        );

        let mut matcher = FeatureFlagMatcher::new(
            "test_user".to_string(),
            team.id,
            team.project_id,
            reader.clone(),
            writer.clone(),
            cohort_cache.clone(),
            None,
            None,
        );

        let result = matcher.get_match(&flag, None, None).await.unwrap();

        assert!(result.matches);
    }

    #[tokio::test]
    async fn test_not_in_cohort_matching() {
        let reader = setup_pg_reader_client(None).await;
        let writer = setup_pg_writer_client(None).await;
        let cohort_cache = Arc::new(CohortCacheManager::new(reader.clone(), None, None));
        let team = insert_new_team_in_pg(reader.clone(), None).await.unwrap();

        // Insert a cohort with a condition that does not match the test user's properties
        let cohort_row = insert_cohort_for_team_in_pg(
            reader.clone(),
            team.id,
            None,
            json!({
                "properties": {
                    "type": "OR",
                    "values": [{
                        "type": "OR",
                        "values": [{
                            "key": "$browser_version",
                            "type": "person",
                            "value": "130",
                            "negation": false,
                            "operator": "gt"
                        }]
                    }]
                }
            }),
            false,
        )
        .await
        .unwrap();

        // Insert a person with properties that do not match the cohort condition
        insert_person_for_team_in_pg(
            reader.clone(),
            team.id,
            "test_user".to_string(),
            Some(json!({"$browser_version": 126})),
        )
        .await
        .unwrap();

        // Define a flag with a NotIn cohort filter
        let flag = create_test_flag(
            None,
            Some(team.id),
            None,
            None,
            Some(FlagFilters {
                groups: vec![FlagGroupType {
                    properties: Some(vec![PropertyFilter {
                        key: "id".to_string(),
                        value: json!(cohort_row.id),
                        operator: Some(OperatorType::NotIn),
                        prop_type: "cohort".to_string(),
                        group_type_index: None,
                        negation: Some(false),
                    }]),
                    rollout_percentage: Some(100.0),
                    variant: None,
                }],
                multivariate: None,
                aggregation_group_type_index: None,
                payloads: None,
                super_groups: None,
                holdout_groups: None,
            }),
            None,
            None,
            None,
        );

        let mut matcher = FeatureFlagMatcher::new(
            "test_user".to_string(),
            team.id,
            team.project_id,
            reader.clone(),
            writer.clone(),
            cohort_cache.clone(),
            None,
            None,
        );

        let result = matcher.get_match(&flag, None, None).await.unwrap();

        assert!(result.matches);
    }

    #[tokio::test]
    async fn test_not_in_cohort_matching_user_in_cohort() {
        let reader = setup_pg_reader_client(None).await;
        let writer = setup_pg_writer_client(None).await;
        let cohort_cache = Arc::new(CohortCacheManager::new(reader.clone(), None, None));
        let team = insert_new_team_in_pg(reader.clone(), None).await.unwrap();

        // Insert a cohort with a condition that matches the test user's properties
        let cohort_row = insert_cohort_for_team_in_pg(
            reader.clone(),
            team.id,
            None,
            json!({
                "properties": {
                    "type": "OR",
                    "values": [{
                        "type": "OR",
                        "values": [{
                            "key": "$browser_version",
                            "type": "person",
                            "value": "125",
                            "negation": false,
                            "operator": "gt"
                        }]
                    }]
                }
            }),
            false,
        )
        .await
        .unwrap();

        // Insert a person with properties that match the cohort condition
        insert_person_for_team_in_pg(
            reader.clone(),
            team.id,
            "test_user".to_string(),
            Some(json!({"$browser_version": 126})),
        )
        .await
        .unwrap();

        // Define a flag with a NotIn cohort filter
        let flag = create_test_flag(
            None,
            Some(team.id),
            None,
            None,
            Some(FlagFilters {
                groups: vec![FlagGroupType {
                    properties: Some(vec![PropertyFilter {
                        key: "id".to_string(),
                        value: json!(cohort_row.id),
                        operator: Some(OperatorType::NotIn),
                        prop_type: "cohort".to_string(),
                        group_type_index: None,
                        negation: Some(false),
                    }]),
                    rollout_percentage: Some(100.0),
                    variant: None,
                }],
                multivariate: None,
                aggregation_group_type_index: None,
                payloads: None,
                super_groups: None,
                holdout_groups: None,
            }),
            None,
            None,
            None,
        );

        let mut matcher = FeatureFlagMatcher::new(
            "test_user".to_string(),
            team.id,
            team.project_id,
            reader.clone(),
            writer.clone(),
            cohort_cache.clone(),
            None,
            None,
        );

        let result = matcher.get_match(&flag, None, None).await.unwrap();

        // The user matches the cohort, but the flag is set to NotIn, so it should evaluate to false
        assert!(!result.matches);
    }

    #[tokio::test]
    async fn test_cohort_dependent_on_another_cohort() {
        let reader = setup_pg_reader_client(None).await;
        let writer = setup_pg_writer_client(None).await;
        let cohort_cache = Arc::new(CohortCacheManager::new(reader.clone(), None, None));
        let team = insert_new_team_in_pg(reader.clone(), None).await.unwrap();

        // Insert a base cohort
        let base_cohort_row = insert_cohort_for_team_in_pg(
            reader.clone(),
            team.id,
            None,
            json!({
                "properties": {
                    "type": "OR",
                    "values": [{
                        "type": "OR",
                        "values": [{
                            "key": "$browser_version",
                            "type": "person",
                            "value": "125",
                            "negation": false,
                            "operator": "gt"
                        }]
                    }]
                }
            }),
            false,
        )
        .await
        .unwrap();

        // Insert a dependent cohort that includes the base cohort
        let dependent_cohort_row = insert_cohort_for_team_in_pg(
            reader.clone(),
            team.id,
            None,
            json!({
                "properties": {
                    "type": "OR",
                    "values": [{
                        "type": "OR",
                        "values": [{
                            "key": "id",
                            "type": "cohort",
                            "value": base_cohort_row.id,
                            "negation": false,
                            "operator": "in"
                        }]
                    }]
                }
            }),
            false,
        )
        .await
        .unwrap();

        // Insert a person with properties that match the base cohort condition
        insert_person_for_team_in_pg(
            reader.clone(),
            team.id,
            "test_user".to_string(),
            Some(json!({"$browser_version": 126})),
        )
        .await
        .unwrap();

        // Define a flag with a cohort filter that depends on another cohort
        let flag = create_test_flag(
            None,
            Some(team.id),
            None,
            None,
            Some(FlagFilters {
                groups: vec![FlagGroupType {
                    properties: Some(vec![PropertyFilter {
                        key: "id".to_string(),
                        value: json!(dependent_cohort_row.id),
                        operator: Some(OperatorType::In),
                        prop_type: "cohort".to_string(),
                        group_type_index: None,
                        negation: Some(false),
                    }]),
                    rollout_percentage: Some(100.0),
                    variant: None,
                }],
                multivariate: None,
                aggregation_group_type_index: None,
                payloads: None,
                super_groups: None,
                holdout_groups: None,
            }),
            None,
            None,
            None,
        );

        let mut matcher = FeatureFlagMatcher::new(
            "test_user".to_string(),
            team.id,
            team.project_id,
            reader.clone(),
            writer.clone(),
            cohort_cache.clone(),
            None,
            None,
        );

        let result = matcher.get_match(&flag, None, None).await.unwrap();

        assert!(result.matches);
    }

    #[tokio::test]
    async fn test_in_cohort_matching_user_not_in_cohort() {
        let reader = setup_pg_reader_client(None).await;
        let writer = setup_pg_writer_client(None).await;
        let cohort_cache = Arc::new(CohortCacheManager::new(reader.clone(), None, None));
        let team = insert_new_team_in_pg(reader.clone(), None).await.unwrap();

        // Insert a cohort with a condition that does not match the test user's properties
        let cohort_row = insert_cohort_for_team_in_pg(
            reader.clone(),
            team.id,
            None,
            json!({
                "properties": {
                    "type": "OR",
                    "values": [{
                        "type": "OR",
                        "values": [{
                            "key": "$browser_version",
                            "type": "person",
                            "value": "130",
                            "negation": false,
                            "operator": "gt"
                        }]
                    }]
                }
            }),
            false,
        )
        .await
        .unwrap();

        // Insert a person with properties that do not match the cohort condition
        insert_person_for_team_in_pg(
            reader.clone(),
            team.id,
            "test_user".to_string(),
            Some(json!({"$browser_version": 125})),
        )
        .await
        .unwrap();

        // Define a flag with an In cohort filter
        let flag = create_test_flag(
            None,
            Some(team.id),
            None,
            None,
            Some(FlagFilters {
                groups: vec![FlagGroupType {
                    properties: Some(vec![PropertyFilter {
                        key: "id".to_string(),
                        value: json!(cohort_row.id),
                        operator: Some(OperatorType::In),
                        prop_type: "cohort".to_string(),
                        group_type_index: None,
                        negation: Some(false),
                    }]),
                    rollout_percentage: Some(100.0),
                    variant: None,
                }],
                multivariate: None,
                aggregation_group_type_index: None,
                payloads: None,
                super_groups: None,
                holdout_groups: None,
            }),
            None,
            None,
            None,
        );

        let mut matcher = FeatureFlagMatcher::new(
            "test_user".to_string(),
            team.id,
            team.project_id,
            reader.clone(),
            writer.clone(),
            cohort_cache.clone(),
            None,
            None,
        );

        let result = matcher.get_match(&flag, None, None).await.unwrap();

        // The user does not match the cohort, and the flag is set to In, so it should evaluate to false
        assert!(!result.matches);
    }

    #[tokio::test]
    async fn test_static_cohort_matching_user_in_cohort() {
        let reader = setup_pg_reader_client(None).await;
        let writer = setup_pg_writer_client(None).await;
        let cohort_cache = Arc::new(CohortCacheManager::new(reader.clone(), None, None));
        let team = insert_new_team_in_pg(reader.clone(), None).await.unwrap();

        // Insert a static cohort
        let cohort = insert_cohort_for_team_in_pg(
            reader.clone(),
            team.id,
            Some("Static Cohort".to_string()),
            json!({}), // Static cohorts don't have property filters
            true,      // is_static = true
        )
        .await
        .unwrap();

        // Insert a person
        let distinct_id = "static_user".to_string();
        insert_person_for_team_in_pg(
            reader.clone(),
            team.id,
            distinct_id.clone(),
            Some(json!({"email": "static@user.com"})),
        )
        .await
        .unwrap();

        // Retrieve the person's ID
        let person_id = get_person_id_by_distinct_id(reader.clone(), team.id, &distinct_id)
            .await
            .unwrap();

        // Associate the person with the static cohort
        add_person_to_cohort(reader.clone(), person_id, cohort.id)
            .await
            .unwrap();

        // Define a flag with an 'In' cohort filter
        let flag = create_test_flag(
            None,
            Some(team.id),
            None,
            None,
            Some(FlagFilters {
                groups: vec![FlagGroupType {
                    properties: Some(vec![PropertyFilter {
                        key: "id".to_string(),
                        value: json!(cohort.id),
                        operator: Some(OperatorType::In),
                        prop_type: "cohort".to_string(),
                        group_type_index: None,
                        negation: Some(false),
                    }]),
                    rollout_percentage: Some(100.0),
                    variant: None,
                }],
                multivariate: None,
                aggregation_group_type_index: None,
                payloads: None,
                super_groups: None,
                holdout_groups: None,
            }),
            None,
            None,
            None,
        );

        let mut matcher = FeatureFlagMatcher::new(
            distinct_id.clone(),
            team.id,
            team.project_id,
            reader.clone(),
            writer.clone(),
            cohort_cache.clone(),
            None,
            None,
        );

        let result = matcher.get_match(&flag, None, None).await.unwrap();

        assert!(
            result.matches,
            "User should match the static cohort and flag"
        );
    }

    #[tokio::test]
    async fn test_static_cohort_matching_user_not_in_cohort() {
        let reader = setup_pg_reader_client(None).await;
        let writer = setup_pg_writer_client(None).await;
        let cohort_cache = Arc::new(CohortCacheManager::new(reader.clone(), None, None));
        let team = insert_new_team_in_pg(reader.clone(), None).await.unwrap();

        // Insert a static cohort
        let cohort = insert_cohort_for_team_in_pg(
            reader.clone(),
            team.id,
            Some("Another Static Cohort".to_string()),
            json!({}), // Static cohorts don't have property filters
            true,
        )
        .await
        .unwrap();

        // Insert a person
        let distinct_id = "non_static_user".to_string();
        insert_person_for_team_in_pg(
            reader.clone(),
            team.id,
            distinct_id.clone(),
            Some(json!({"email": "nonstatic@user.com"})),
        )
        .await
        .unwrap();

        // Note: Do NOT associate the person with the static cohort

        // Define a flag with an 'In' cohort filter
        let flag = create_test_flag(
            None,
            Some(team.id),
            None,
            None,
            Some(FlagFilters {
                groups: vec![FlagGroupType {
                    properties: Some(vec![PropertyFilter {
                        key: "id".to_string(),
                        value: json!(cohort.id),
                        operator: Some(OperatorType::In),
                        prop_type: "cohort".to_string(),
                        group_type_index: None,
                        negation: Some(false),
                    }]),
                    rollout_percentage: Some(100.0),
                    variant: None,
                }],
                multivariate: None,
                aggregation_group_type_index: None,
                payloads: None,
                super_groups: None,
                holdout_groups: None,
            }),
            None,
            None,
            None,
        );

        let mut matcher = FeatureFlagMatcher::new(
            distinct_id.clone(),
            team.id,
            team.project_id,
            reader.clone(),
            writer.clone(),
            cohort_cache.clone(),
            None,
            None,
        );

        let result = matcher.get_match(&flag, None, None).await.unwrap();

        assert!(
            !result.matches,
            "User should not match the static cohort and flag"
        );
    }

    #[tokio::test]
    async fn test_static_cohort_not_in_matching_user_not_in_cohort() {
        let reader = setup_pg_reader_client(None).await;
        let writer = setup_pg_writer_client(None).await;
        let cohort_cache = Arc::new(CohortCacheManager::new(reader.clone(), None, None));
        let team = insert_new_team_in_pg(reader.clone(), None).await.unwrap();

        // Insert a static cohort
        let cohort = insert_cohort_for_team_in_pg(
            reader.clone(),
            team.id,
            Some("Static Cohort NotIn".to_string()),
            json!({}), // Static cohorts don't have property filters
            true,      // is_static = true
        )
        .await
        .unwrap();

        // Insert a person
        let distinct_id = "not_in_static_user".to_string();
        insert_person_for_team_in_pg(
            reader.clone(),
            team.id,
            distinct_id.clone(),
            Some(json!({"email": "notinstatic@user.com"})),
        )
        .await
        .unwrap();

        // No association with the static cohort

        // Define a flag with a 'NotIn' cohort filter
        let flag = create_test_flag(
            None,
            Some(team.id),
            None,
            None,
            Some(FlagFilters {
                groups: vec![FlagGroupType {
                    properties: Some(vec![PropertyFilter {
                        key: "id".to_string(),
                        value: json!(cohort.id),
                        operator: Some(OperatorType::NotIn),
                        prop_type: "cohort".to_string(),
                        group_type_index: None,
                        negation: Some(false),
                    }]),
                    rollout_percentage: Some(100.0),
                    variant: None,
                }],
                multivariate: None,
                aggregation_group_type_index: None,
                payloads: None,
                super_groups: None,
                holdout_groups: None,
            }),
            None,
            None,
            None,
        );

        let mut matcher = FeatureFlagMatcher::new(
            distinct_id.clone(),
            team.id,
            team.project_id,
            reader.clone(),
            writer.clone(),
            cohort_cache.clone(),
            None,
            None,
        );

        let result = matcher.get_match(&flag, None, None).await.unwrap();

        assert!(
            result.matches,
            "User not in the static cohort should match the 'NotIn' flag"
        );
    }

    #[tokio::test]
    async fn test_static_cohort_not_in_matching_user_in_cohort() {
        let reader = setup_pg_reader_client(None).await;
        let writer = setup_pg_writer_client(None).await;
        let cohort_cache = Arc::new(CohortCacheManager::new(reader.clone(), None, None));
        let team = insert_new_team_in_pg(reader.clone(), None).await.unwrap();

        // Insert a static cohort
        let cohort = insert_cohort_for_team_in_pg(
            reader.clone(),
            team.id,
            Some("Static Cohort NotIn User In".to_string()),
            json!({}), // Static cohorts don't have property filters
            true,      // is_static = true
        )
        .await
        .unwrap();

        // Insert a person
        let distinct_id = "in_not_in_static_user".to_string();
        insert_person_for_team_in_pg(
            reader.clone(),
            team.id,
            distinct_id.clone(),
            Some(json!({"email": "innotinstatic@user.com"})),
        )
        .await
        .unwrap();

        // Retrieve the person's ID
        let person_id = get_person_id_by_distinct_id(reader.clone(), team.id, &distinct_id)
            .await
            .unwrap();

        // Associate the person with the static cohort
        add_person_to_cohort(reader.clone(), person_id, cohort.id)
            .await
            .unwrap();

        // Define a flag with a 'NotIn' cohort filter
        let flag = create_test_flag(
            None,
            Some(team.id),
            None,
            None,
            Some(FlagFilters {
                groups: vec![FlagGroupType {
                    properties: Some(vec![PropertyFilter {
                        key: "id".to_string(),
                        value: json!(cohort.id),
                        operator: Some(OperatorType::NotIn),
                        prop_type: "cohort".to_string(),
                        group_type_index: None,
                        negation: Some(false),
                    }]),
                    rollout_percentage: Some(100.0),
                    variant: None,
                }],
                multivariate: None,
                aggregation_group_type_index: None,
                payloads: None,
                super_groups: None,
                holdout_groups: None,
            }),
            None,
            None,
            None,
        );

        let mut matcher = FeatureFlagMatcher::new(
            distinct_id.clone(),
            team.id,
            team.project_id,
            reader.clone(),
            writer.clone(),
            cohort_cache.clone(),
            None,
            None,
        );

        let result = matcher.get_match(&flag, None, None).await.unwrap();

        assert!(
            !result.matches,
            "User in the static cohort should not match the 'NotIn' flag"
        );
    }

    #[tokio::test]
    async fn test_set_feature_flag_hash_key_overrides_success() {
        let reader = setup_pg_reader_client(None).await;
        let writer = setup_pg_writer_client(None).await;
        let team = insert_new_team_in_pg(reader.clone(), None).await.unwrap();
        let distinct_id = "user2".to_string();

        // Insert person
        insert_person_for_team_in_pg(reader.clone(), team.id, distinct_id.clone(), None)
            .await
            .unwrap();

        // Create a feature flag with ensure_experience_continuity = true
        let flag = create_test_flag(
            None,
            Some(team.id),
            Some("Test Flag".to_string()),
            Some("test_flag".to_string()),
            Some(FlagFilters {
                groups: vec![],
                multivariate: None,
                aggregation_group_type_index: None,
                payloads: None,
                super_groups: None,
                holdout_groups: None,
            }),
            Some(false), // not deleted
            Some(true),  // active
            Some(true),  // ensure_experience_continuity
        );

        // Convert flag to FeatureFlagRow
        let flag_row = FeatureFlagRow {
            id: flag.id,
            team_id: flag.team_id,
            name: flag.name,
            key: flag.key,
            filters: json!(flag.filters),
            deleted: flag.deleted,
            active: flag.active,
            ensure_experience_continuity: flag.ensure_experience_continuity,
            version: flag.version,
        };

        // Insert the feature flag into the database
        insert_flag_for_team_in_pg(writer.clone(), team.id, Some(flag_row))
            .await
            .unwrap();

        // Set hash key override
        set_feature_flag_hash_key_overrides(
            writer.clone(),
            team.id,
            vec![distinct_id.clone()],
            team.project_id,
            "hash_key_2".to_string(),
        )
        .await
        .unwrap();

        // Retrieve hash key overrides
        let overrides =
            get_feature_flag_hash_key_overrides(reader.clone(), team.id, vec![distinct_id.clone()])
                .await
                .unwrap();

        assert_eq!(
            overrides.get("test_flag"),
            Some(&"hash_key_2".to_string()),
            "Hash key override should match the set value"
        );
    }

    #[tokio::test]
    async fn test_get_feature_flag_hash_key_overrides_success() {
        let reader = setup_pg_reader_client(None).await;
        let writer = setup_pg_writer_client(None).await;
        let team = insert_new_team_in_pg(reader.clone(), None).await.unwrap();
        let distinct_id = "user2".to_string();

        // Insert person
        insert_person_for_team_in_pg(reader.clone(), team.id, distinct_id.clone(), None)
            .await
            .unwrap();

        // Create a feature flag with ensure_experience_continuity = true
        let flag = create_test_flag(
            None,
            Some(team.id),
            Some("Test Flag".to_string()),
            Some("test_flag".to_string()),
            Some(FlagFilters {
                groups: vec![],
                multivariate: None,
                aggregation_group_type_index: None,
                payloads: None,
                super_groups: None,
                holdout_groups: None,
            }),
            Some(false), // not deleted
            Some(true),  // active
            Some(true),  // ensure_experience_continuity
        );

        // Convert flag to FeatureFlagRow
        let flag_row = FeatureFlagRow {
            id: flag.id,
            team_id: flag.team_id,
            name: flag.name,
            key: flag.key,
            filters: json!(flag.filters),
            deleted: flag.deleted,
            active: flag.active,
            ensure_experience_continuity: flag.ensure_experience_continuity,
            version: flag.version,
        };

        // Insert the feature flag into the database
        insert_flag_for_team_in_pg(writer.clone(), team.id, Some(flag_row))
            .await
            .unwrap();

        // Set hash key override
        set_feature_flag_hash_key_overrides(
            writer.clone(),
            team.id,
            vec![distinct_id.clone()],
            team.project_id,
            "hash_key_2".to_string(),
        )
        .await
        .unwrap();

        // Retrieve hash key overrides
        let overrides =
            get_feature_flag_hash_key_overrides(reader.clone(), team.id, vec![distinct_id.clone()])
                .await
                .unwrap();

        assert_eq!(
            overrides.get("test_flag"),
            Some(&"hash_key_2".to_string()),
            "Hash key override should match the set value"
        );
    }

    #[tokio::test]
    async fn test_evaluate_feature_flags_with_experience_continuity() {
        let reader = setup_pg_reader_client(None).await;
        let writer = setup_pg_writer_client(None).await;
        let cohort_cache = Arc::new(CohortCacheManager::new(reader.clone(), None, None));
        let team = insert_new_team_in_pg(reader.clone(), None).await.unwrap();
        let distinct_id = "user3".to_string();

        // Insert person
        insert_person_for_team_in_pg(
            reader.clone(),
            team.id,
            distinct_id.clone(),
            Some(json!({"email": "user3@example.com"})),
        )
        .await
        .unwrap();

        // Create flag with experience continuity
        let flag = create_test_flag(
            None,
            Some(team.id),
            None,
            Some("flag_continuity".to_string()),
            Some(FlagFilters {
                groups: vec![FlagGroupType {
                    properties: Some(vec![PropertyFilter {
                        key: "email".to_string(),
                        value: json!("user3@example.com"),
                        operator: None,
                        prop_type: "person".to_string(),
                        group_type_index: None,
                        negation: None,
                    }]),
                    rollout_percentage: Some(100.0),
                    variant: None,
                }],
                multivariate: None,
                aggregation_group_type_index: None,
                payloads: None,
                super_groups: None,
                holdout_groups: None,
            }),
            None,
            None,
            Some(true),
        );

        // Set hash key override
        set_feature_flag_hash_key_overrides(
            writer.clone(),
            team.id,
            vec![distinct_id.clone()],
            team.project_id,
            "hash_key_continuity".to_string(),
        )
        .await
        .unwrap();

        let flags = FeatureFlagList {
            flags: vec![flag.clone()],
        };

        let result = FeatureFlagMatcher::new(
            distinct_id.clone(),
            team.id,
            team.project_id,
            reader.clone(),
            writer.clone(),
            cohort_cache.clone(),
            None,
            None,
        )
        .evaluate_all_feature_flags(flags, None, None, Some("hash_key_continuity".to_string()))
        .await;

        let legacy_response = LegacyFlagsResponse::from_response(result);
        assert!(
            !legacy_response.errors_while_computing_flags,
            "No error should occur"
        );
        assert_eq!(
            legacy_response.feature_flags.get("flag_continuity"),
            Some(&FlagValue::Boolean(true)),
            "Flag should be evaluated as true with continuity"
        );
    }

    #[tokio::test]
    async fn test_evaluate_feature_flags_with_continuity_missing_override() {
        let reader = setup_pg_reader_client(None).await;
        let writer = setup_pg_writer_client(None).await;
        let cohort_cache = Arc::new(CohortCacheManager::new(reader.clone(), None, None));
        let team = insert_new_team_in_pg(reader.clone(), None).await.unwrap();
        let distinct_id = "user4".to_string();

        insert_person_for_team_in_pg(
            reader.clone(),
            team.id,
            distinct_id.clone(),
            Some(json!({"email": "user4@example.com"})),
        )
        .await
        .unwrap();

        // Create flag with experience continuity
        let flag = create_test_flag(
            None,
            Some(team.id),
            None,
            Some("flag_continuity_missing".to_string()),
            Some(FlagFilters {
                groups: vec![FlagGroupType {
                    properties: Some(vec![PropertyFilter {
                        key: "email".to_string(),
                        value: json!("user4@example.com"),
                        operator: None,
                        prop_type: "person".to_string(),
                        group_type_index: None,
                        negation: None,
                    }]),
                    rollout_percentage: Some(100.0),
                    variant: None,
                }],
                multivariate: None,
                aggregation_group_type_index: None,
                payloads: None,
                super_groups: None,
                holdout_groups: None,
            }),
            None,
            None,
            Some(true),
        );

        let flags = FeatureFlagList {
            flags: vec![flag.clone()],
        };

        let result = FeatureFlagMatcher::new(
            distinct_id.clone(),
            team.id,
            team.project_id,
            reader.clone(),
            writer.clone(),
            cohort_cache.clone(),
            None,
            None,
        )
        .evaluate_all_feature_flags(flags, None, None, None)
        .await;

        println!("result: {:?}", result);

        assert!(result.flags.get("flag_continuity_missing").unwrap().enabled);

        let legacy_response = LegacyFlagsResponse::from_response(result);
        assert!(
            !legacy_response.errors_while_computing_flags,
            "No error should occur"
        );
        assert_eq!(
            legacy_response.feature_flags.get("flag_continuity_missing"),
            Some(&FlagValue::Boolean(true)),
            "Flag should be evaluated as true even without continuity override"
        );
    }

    #[tokio::test]
    async fn test_evaluate_all_feature_flags_mixed_continuity() {
        let reader = setup_pg_reader_client(None).await;
        let writer = setup_pg_writer_client(None).await;
        let cohort_cache = Arc::new(CohortCacheManager::new(reader.clone(), None, None));
        let team = insert_new_team_in_pg(reader.clone(), None).await.unwrap();
        let distinct_id = "user5".to_string();

        insert_person_for_team_in_pg(
            reader.clone(),
            team.id,
            distinct_id.clone(),
            Some(json!({"email": "user5@example.com"})),
        )
        .await
        .unwrap();

        // Create flag with continuity
        let flag_continuity = create_test_flag(
            None,
            Some(team.id),
            None,
            Some("flag_continuity_mix".to_string()),
            Some(FlagFilters {
                groups: vec![FlagGroupType {
                    properties: Some(vec![PropertyFilter {
                        key: "email".to_string(),
                        value: json!("user5@example.com"),
                        operator: None,
                        prop_type: "person".to_string(),
                        group_type_index: None,
                        negation: None,
                    }]),
                    rollout_percentage: Some(100.0),
                    variant: None,
                }],
                multivariate: None,
                aggregation_group_type_index: None,
                payloads: None,
                super_groups: None,
                holdout_groups: None,
            }),
            None,
            None,
            Some(true),
        );

        // Create flag without continuity
        let flag_no_continuity = create_test_flag(
            None,
            Some(team.id),
            None,
            Some("flag_no_continuity_mix".to_string()),
            Some(FlagFilters {
                groups: vec![FlagGroupType {
                    properties: Some(vec![PropertyFilter {
                        key: "age".to_string(),
                        value: json!(30),
                        operator: Some(OperatorType::Gt),
                        prop_type: "person".to_string(),
                        group_type_index: None,
                        negation: None,
                    }]),
                    rollout_percentage: Some(100.0),
                    variant: None,
                }],
                multivariate: None,
                aggregation_group_type_index: None,
                payloads: None,
                super_groups: None,
                holdout_groups: None,
            }),
            None,
            None,
            Some(false),
        );

        // Set hash key override for the continuity flag
        set_feature_flag_hash_key_overrides(
            writer.clone(),
            team.id,
            vec![distinct_id.clone()],
            team.project_id,
            "hash_key_mixed".to_string(),
        )
        .await
        .unwrap();

        let flags = FeatureFlagList {
            flags: vec![flag_continuity.clone(), flag_no_continuity.clone()],
        };

        let result = FeatureFlagMatcher::new(
            distinct_id.clone(),
            team.id,
            team.project_id,
            reader.clone(),
            writer.clone(),
            cohort_cache.clone(),
            None,
            None,
        )
        .evaluate_all_feature_flags(
            flags,
            Some(HashMap::from([("age".to_string(), json!(35))])),
            None,
            Some("hash_key_mixed".to_string()),
        )
        .await;

        let legacy_response = LegacyFlagsResponse::from_response(result);
        assert!(
            !legacy_response.errors_while_computing_flags,
            "No error should occur"
        );
        assert_eq!(
            legacy_response.feature_flags.get("flag_continuity_mix"),
            Some(&FlagValue::Boolean(true)),
            "Continuity flag should be evaluated as true"
        );
        assert_eq!(
            legacy_response.feature_flags.get("flag_no_continuity_mix"),
            Some(&FlagValue::Boolean(true)),
            "Non-continuity flag should be evaluated based on properties"
        );
    }

    #[tokio::test]
    async fn test_variant_override_in_condition() {
        let reader = setup_pg_reader_client(None).await;
        let writer = setup_pg_writer_client(None).await;
        let cohort_cache = Arc::new(CohortCacheManager::new(reader.clone(), None, None));
        let team = insert_new_team_in_pg(reader.clone(), None).await.unwrap();
        let distinct_id = "test_user".to_string();

        // Insert a person with properties that will match our condition
        insert_person_for_team_in_pg(
            reader.clone(),
            team.id,
            distinct_id.clone(),
            Some(json!({"email": "test@example.com"})),
        )
        .await
        .unwrap();

        // Create a flag with multiple variants and a condition with a variant override
        let flag = create_test_flag(
            None,
            Some(team.id),
            None,
            Some("test_flag".to_string()),
            Some(FlagFilters {
                groups: vec![FlagGroupType {
                    properties: Some(vec![PropertyFilter {
                        key: "email".to_string(),
                        value: json!("test@example.com"),
                        operator: None,
                        prop_type: "person".to_string(),
                        group_type_index: None,
                        negation: None,
                    }]),
                    rollout_percentage: Some(100.0),
                    variant: Some("control".to_string()), // Override to always show "control" variant
                }],
                multivariate: Some(MultivariateFlagOptions {
                    variants: vec![
                        MultivariateFlagVariant {
                            name: Some("Control".to_string()),
                            key: "control".to_string(),
                            rollout_percentage: 25.0,
                        },
                        MultivariateFlagVariant {
                            name: Some("Test".to_string()),
                            key: "test".to_string(),
                            rollout_percentage: 25.0,
                        },
                        MultivariateFlagVariant {
                            name: Some("Test2".to_string()),
                            key: "test2".to_string(),
                            rollout_percentage: 50.0,
                        },
                    ],
                }),
                aggregation_group_type_index: None,
                payloads: None,
                super_groups: None,
                holdout_groups: None,
            }),
            None,
            None,
            None,
        );

        let mut matcher = FeatureFlagMatcher::new(
            distinct_id.clone(),
            team.id,
            team.project_id,
            reader.clone(),
            writer.clone(),
            cohort_cache.clone(),
            None,
            None,
        );

        let result = matcher.get_match(&flag, None, None).await.unwrap();

        // The condition matches and has a variant override, so it should return "control"
        // regardless of what the hash-based variant computation would return
        assert!(result.matches);
        assert_eq!(result.variant, Some("control".to_string()));

        // Now test with an invalid variant override
        let flag_invalid_override = create_test_flag(
            None,
            Some(team.id),
            None,
            Some("test_flag_invalid".to_string()),
            Some(FlagFilters {
                groups: vec![FlagGroupType {
                    properties: Some(vec![PropertyFilter {
                        key: "email".to_string(),
                        value: json!("test@example.com"),
                        operator: None,
                        prop_type: "person".to_string(),
                        group_type_index: None,
                        negation: None,
                    }]),
                    rollout_percentage: Some(100.0),
                    variant: Some("nonexistent_variant".to_string()), // Override with invalid variant
                }],
                multivariate: Some(MultivariateFlagOptions {
                    variants: vec![
                        MultivariateFlagVariant {
                            name: Some("Control".to_string()),
                            key: "control".to_string(),
                            rollout_percentage: 25.0,
                        },
                        MultivariateFlagVariant {
                            name: Some("Test".to_string()),
                            key: "test".to_string(),
                            rollout_percentage: 75.0,
                        },
                    ],
                }),
                aggregation_group_type_index: None,
                payloads: None,
                super_groups: None,
                holdout_groups: None,
            }),
            None,
            None,
            None,
        );

        let result_invalid = matcher
            .get_match(&flag_invalid_override, None, None)
            .await
            .unwrap();

        // The condition matches but has an invalid variant override,
        // so it should fall back to hash-based variant computation
        assert!(result_invalid.matches);
        assert!(result_invalid.variant.is_some()); // Will be either "control" or "test" based on hash
    }

    #[tokio::test]
    async fn test_feature_flag_with_holdout_filter() {
        let reader = setup_pg_reader_client(None).await;
        let writer = setup_pg_writer_client(None).await;
        let cohort_cache = Arc::new(CohortCacheManager::new(reader.clone(), None, None));
        let team = insert_new_team_in_pg(reader.clone(), None).await.unwrap();

        // example_id is outside 70% holdout
        let _person1 = insert_person_for_team_in_pg(
            reader.clone(),
            team.id,
            "example_id".to_string(),
            Some(json!({"$some_prop": 5})),
        )
        .await
        .unwrap();

        // example_id2 is within 70% holdout
        let _person2 = insert_person_for_team_in_pg(
            reader.clone(),
            team.id,
            "example_id2".to_string(),
            Some(json!({"$some_prop": 5})),
        )
        .await
        .unwrap();

        let multivariate_json = MultivariateFlagOptions {
            variants: vec![
                MultivariateFlagVariant {
                    key: "first-variant".to_string(),
                    name: Some("First Variant".to_string()),
                    rollout_percentage: 50.0,
                },
                MultivariateFlagVariant {
                    key: "second-variant".to_string(),
                    name: Some("Second Variant".to_string()),
                    rollout_percentage: 25.0,
                },
                MultivariateFlagVariant {
                    key: "third-variant".to_string(),
                    name: Some("Third Variant".to_string()),
                    rollout_percentage: 25.0,
                },
            ],
        };

        let flag_with_holdout = create_test_flag(
            Some(1),
            Some(team.id),
            Some("Flag with holdout".to_string()),
            Some("flag-with-gt-filter".to_string()),
            Some(FlagFilters {
                groups: vec![FlagGroupType {
                    properties: Some(vec![PropertyFilter {
                        key: "$some_prop".to_string(),
                        value: json!(4),
                        operator: Some(OperatorType::Gt),
                        prop_type: "person".to_string(),
                        group_type_index: None,
                        negation: None,
                    }]),
                    rollout_percentage: Some(100.0),
                    variant: None,
                }],
                holdout_groups: Some(vec![FlagGroupType {
                    properties: Some(vec![]),
                    rollout_percentage: Some(70.0),
                    variant: Some("holdout".to_string()),
                }]),
                multivariate: Some(multivariate_json.clone()),
                aggregation_group_type_index: None,
                payloads: None,
                super_groups: None,
            }),
            None,
            Some(true),
            None,
        );

        let other_flag_with_holdout = create_test_flag(
            Some(2),
            Some(team.id),
            Some("Other flag with holdout".to_string()),
            Some("other-flag-with-gt-filter".to_string()),
            Some(FlagFilters {
                groups: vec![FlagGroupType {
                    properties: Some(vec![PropertyFilter {
                        key: "$some_prop".to_string(),
                        value: json!(4),
                        operator: Some(OperatorType::Gt),
                        prop_type: "person".to_string(),
                        group_type_index: None,
                        negation: None,
                    }]),
                    rollout_percentage: Some(100.0),
                    variant: None,
                }],
                holdout_groups: Some(vec![FlagGroupType {
                    properties: Some(vec![]),
                    rollout_percentage: Some(70.0),
                    variant: Some("holdout".to_string()),
                }]),
                multivariate: Some(multivariate_json.clone()),
                aggregation_group_type_index: None,
                payloads: None,
                super_groups: None,
            }),
            None,
            Some(true),
            None,
        );

        let flag_without_holdout = create_test_flag(
            Some(3),
            Some(team.id),
            Some("Flag".to_string()),
            Some("other-flag-without-holdout-with-gt-filter".to_string()),
            Some(FlagFilters {
                groups: vec![FlagGroupType {
                    properties: Some(vec![PropertyFilter {
                        key: "$some_prop".to_string(),
                        value: json!(4),
                        operator: Some(OperatorType::Gt),
                        prop_type: "person".to_string(),
                        group_type_index: None,
                        negation: None,
                    }]),
                    rollout_percentage: Some(100.0),
                    variant: None,
                }],
                holdout_groups: Some(vec![FlagGroupType {
                    properties: Some(vec![]),
                    rollout_percentage: Some(0.0),
                    variant: Some("holdout".to_string()),
                }]),
                multivariate: Some(multivariate_json),
                aggregation_group_type_index: None,
                payloads: None,
                super_groups: None,
            }),
            None,
            Some(true),
            None,
        );

        // regular flag evaluation when outside holdout
        let mut matcher = FeatureFlagMatcher::new(
            "example_id".to_string(),
            team.id,
            team.project_id,
            reader.clone(),
            writer.clone(),
            cohort_cache.clone(),
            None,
            None,
        );

        let result = matcher
            .get_match(&flag_with_holdout, None, None)
            .await
            .unwrap();
        assert!(result.matches);
        assert_eq!(result.variant, Some("second-variant".to_string()));
        assert_eq!(result.reason, FeatureFlagMatchReason::ConditionMatch);

        // Test inside holdout behavior - should get holdout variant override
        let mut matcher2 = FeatureFlagMatcher::new(
            "example_id2".to_string(),
            team.id,
            team.project_id,
            reader.clone(),
            writer.clone(),
            cohort_cache.clone(),
            None,
            None,
        );

        let result = matcher2
            .get_match(&flag_with_holdout, None, None)
            .await
            .unwrap();

        assert!(result.matches);
        assert_eq!(result.variant, Some("holdout".to_string()));
        assert_eq!(result.reason, FeatureFlagMatchReason::HoldoutConditionValue);

        // same should hold true for a different feature flag when within holdout
        let result = matcher2
            .get_match(&other_flag_with_holdout, None, None)
            .await
            .unwrap();
        assert!(result.matches);
        assert_eq!(result.variant, Some("holdout".to_string()));
        assert_eq!(result.reason, FeatureFlagMatchReason::HoldoutConditionValue);

        // Test with matcher1 (outside holdout) to verify different variants
        let result = matcher
            .get_match(&other_flag_with_holdout, None, None)
            .await
            .unwrap();
        assert!(result.matches);
        assert_eq!(result.variant, Some("third-variant".to_string()));
        assert_eq!(result.reason, FeatureFlagMatchReason::ConditionMatch);

        // when holdout exists but is zero, should default to regular flag evaluation
        let result = matcher
            .get_match(&flag_without_holdout, None, None)
            .await
            .unwrap();
        assert!(result.matches);
        assert_eq!(result.variant, Some("second-variant".to_string()));
        assert_eq!(result.reason, FeatureFlagMatchReason::ConditionMatch);

        let result = matcher2
            .get_match(&flag_without_holdout, None, None)
            .await
            .unwrap();
        assert!(result.matches);
        assert_eq!(result.variant, Some("second-variant".to_string()));
        assert_eq!(result.reason, FeatureFlagMatchReason::ConditionMatch);
    }

    #[rstest]
    #[case("some_distinct_id", 0.7270002403585725)]
    #[case("test-identifier", 0.4493881716040236)]
    #[case("example_id", 0.9402003475831224)]
    #[case("example_id2", 0.6292740389966519)]
    #[tokio::test]
    async fn test_calculate_hash(#[case] hashed_identifier: &str, #[case] expected_hash: f64) {
        let hash = calculate_hash("holdout-", hashed_identifier, "")
            .await
            .unwrap();
        assert!(
            (hash - expected_hash).abs() < f64::EPSILON,
            "Hash {} should equal expected value {} within floating point precision",
            hash,
            expected_hash
        );
    }

    #[tokio::test]
    async fn test_variants() {
        // Ported from posthog/test/test_feature_flag.py test_variants
        let reader = setup_pg_reader_client(None).await;
        let writer = setup_pg_writer_client(None).await;
        let cohort_cache = Arc::new(CohortCacheManager::new(reader.clone(), None, None));
        let team = insert_new_team_in_pg(reader.clone(), None).await.unwrap();

        let flag = FeatureFlag {
            id: 1,
            team_id: team.id,
            name: Some("Beta feature".to_string()),
            key: "beta-feature".to_string(),
            filters: FlagFilters {
                groups: vec![FlagGroupType {
                    properties: None,
                    rollout_percentage: None,
                    variant: None,
                }],
                multivariate: Some(MultivariateFlagOptions {
                    variants: vec![
                        MultivariateFlagVariant {
                            name: Some("First Variant".to_string()),
                            key: "first-variant".to_string(),
                            rollout_percentage: 50.0,
                        },
                        MultivariateFlagVariant {
                            name: Some("Second Variant".to_string()),
                            key: "second-variant".to_string(),
                            rollout_percentage: 25.0,
                        },
                        MultivariateFlagVariant {
                            name: Some("Third Variant".to_string()),
                            key: "third-variant".to_string(),
                            rollout_percentage: 25.0,
                        },
                    ],
                }),
                aggregation_group_type_index: None,
                payloads: None,
                super_groups: None,
                holdout_groups: None,
            },
            deleted: false,
            active: true,
            ensure_experience_continuity: false,
            version: Some(1),
        };

        // Test user "11" - should get first-variant
        let mut matcher = FeatureFlagMatcher::new(
            "11".to_string(),
            team.id,
            team.project_id,
            reader.clone(),
            writer.clone(),
            cohort_cache.clone(),
            None,
            None,
        );
        let result = matcher.get_match(&flag, None, None).await.unwrap();
        assert_eq!(
            result,
            FeatureFlagMatch {
                matches: true,
                variant: Some("first-variant".to_string()),
                reason: FeatureFlagMatchReason::ConditionMatch,
                condition_index: Some(0),
                payload: None,
            }
        );

        // Test user "example_id" - should get second-variant
        let mut matcher = FeatureFlagMatcher::new(
            "example_id".to_string(),
            team.id,
            team.project_id,
            reader.clone(),
            writer.clone(),
            cohort_cache.clone(),
            None,
            None,
        );
        let result = matcher.get_match(&flag, None, None).await.unwrap();
        assert_eq!(
            result,
            FeatureFlagMatch {
                matches: true,
                variant: Some("second-variant".to_string()),
                reason: FeatureFlagMatchReason::ConditionMatch,
                condition_index: Some(0),
                payload: None,
            }
        );

        // Test user "3" - should get third-variant
        let mut matcher = FeatureFlagMatcher::new(
            "3".to_string(),
            team.id,
            team.project_id,
            reader.clone(),
            writer.clone(),
            cohort_cache.clone(),
            None,
            None,
        );
        let result = matcher.get_match(&flag, None, None).await.unwrap();
        assert_eq!(
            result,
            FeatureFlagMatch {
                matches: true,
                variant: Some("third-variant".to_string()),
                reason: FeatureFlagMatchReason::ConditionMatch,
                condition_index: Some(0),
                payload: None,
            }
        );
    }

    #[tokio::test]
    async fn test_static_cohort_evaluation_skips_dependency_graph() {
        let reader = setup_pg_reader_client(None).await;
        let writer = setup_pg_writer_client(None).await;
        let cohort_cache = Arc::new(CohortCacheManager::new(reader.clone(), None, None));
        let team = insert_new_team_in_pg(reader.clone(), None).await.unwrap();

        // Insert a static cohort
        let cohort = insert_cohort_for_team_in_pg(
            reader.clone(),
            team.id,
            Some("Static Cohort".to_string()),
            json!({}), // Static cohorts don't have property filters
            true,      // is_static = true
        )
        .await
        .unwrap();

        // Insert a person
        let distinct_id = "static_user".to_string();
        insert_person_for_team_in_pg(
            reader.clone(),
            team.id,
            distinct_id.clone(),
            Some(json!({"email": "static@user.com"})),
        )
        .await
        .unwrap();

        // Get person ID and add to cohort
        let person_id = get_person_id_by_distinct_id(reader.clone(), team.id, &distinct_id)
            .await
            .unwrap();
        add_person_to_cohort(reader.clone(), person_id, cohort.id)
            .await
            .unwrap();

        // Define a flag that references the static cohort
        let flag = create_test_flag(
            None,
            Some(team.id),
            None,
            None,
            Some(FlagFilters {
                groups: vec![FlagGroupType {
                    properties: Some(vec![PropertyFilter {
                        key: "id".to_string(),
                        value: json!(cohort.id),
                        operator: Some(OperatorType::In),
                        prop_type: "cohort".to_string(),
                        group_type_index: None,
                        negation: Some(false),
                    }]),
                    rollout_percentage: Some(100.0),
                    variant: None,
                }],
                multivariate: None,
                aggregation_group_type_index: None,
                payloads: None,
                super_groups: None,
                holdout_groups: None,
            }),
            None,
            None,
            None,
        );

        let mut matcher = FeatureFlagMatcher::new(
            distinct_id.clone(),
            team.id,
            team.project_id,
            reader.clone(),
            writer.clone(),
            cohort_cache.clone(),
            None,
            None,
        );

        // This should not throw CohortNotFound because we skip dependency graph evaluation for static cohorts
        let result = matcher.get_match(&flag, None, None).await;
        assert!(result.is_ok(), "Should not throw CohortNotFound error");

        let match_result = result.unwrap();
        assert!(match_result.matches, "User should match the static cohort");
    }
}
