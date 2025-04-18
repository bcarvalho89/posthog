import asyncio
import collections.abc
import contextlib
import datetime as dt
import json
import ssl
import typing
import uuid
from urllib.parse import urljoin

import aiohttp
import pyarrow as pa
import requests
import structlog
from django.conf import settings

import posthog.temporal.common.asyncpa as asyncpa
from posthog.temporal.common.logger import get_internal_logger

logger = structlog.get_logger()


def encode_clickhouse_data(data: typing.Any, quote_char="'") -> bytes:
    """Encode data for ClickHouse.

    Depending on the type of data the encoding is different.

    Returns:
        The encoded bytes.
    """
    match data:
        case None:
            return b"NULL"

        case uuid.UUID():
            return f"{quote_char}{data}{quote_char}".encode()

        case int() | float():
            if isinstance(data, float) and data.is_integer():
                return f"{int(data)}".encode()
            return f"{data}".encode()

        case dt.datetime():
            timezone_arg = ""
            if data.tzinfo:
                timezone_arg = f", '{data:%Z}'"

            if data.microsecond == 0:
                return f"toDateTime('{data:%Y-%m-%d %H:%M:%S}'{timezone_arg})".encode()
            return f"toDateTime64('{data:%Y-%m-%d %H:%M:%S.%f}', 6{timezone_arg})".encode()

        case list():
            encoded_data = [encode_clickhouse_data(value) for value in data]
            result = b"[" + b",".join(encoded_data) + b"]"
            return result

        case tuple():
            encoded_data = [encode_clickhouse_data(value) for value in data]
            result = b"(" + b",".join(encoded_data) + b")"
            return result

        case dict():
            # Encode dictionaries as JSON, as it can represent a Python dictionary in a way ClickHouse understands.
            # This means INSERT queries with dictionary data are only supported with 'FORMAT JSONEachRow', which
            # is enough for now as most if not all of our INSERT query workloads are in unit test setup.
            encoded_data = []
            quote_char = '"'  # JSON requires double quotes.

            for key, value in data.items():
                if isinstance(value, dt.datetime):
                    value = str(value.timestamp())
                elif isinstance(value, uuid.UUID) or isinstance(value, str):
                    value = str(value)

                encoded_data.append(
                    f'"{str(key)}"'.encode() + b":" + encode_clickhouse_data(value, quote_char=quote_char)
                )

            result = b"{" + b",".join(encoded_data) + b"}"
            return result

        case _:
            str_data = str(data)
            str_data = str_data.replace("\\", "\\\\").replace("'", "\\'")
            return f"{quote_char}{str_data}{quote_char}".encode()


class ChunkBytesAsyncStreamIterator:
    """Async iterator of HTTP chunk bytes.

    Similar to the class provided by aiohttp, but this allows us to control
    when to stop iteration.
    """

    def __init__(self, stream: aiohttp.StreamReader) -> None:
        self._stream = stream

    def __aiter__(self) -> "ChunkBytesAsyncStreamIterator":
        return self

    async def __anext__(self) -> bytes:
        data, end_of_chunk = await self._stream.readchunk()

        if data == b"" and end_of_chunk is False and self._stream.at_eof():
            await logger.adebug("At EOF, stopping chunk iteration")
            raise StopAsyncIteration

        return data


class ClickHouseClientNotConnected(Exception):
    """Exception raised when attempting to run an async query without connecting."""

    def __init__(self):
        super().__init__("ClickHouseClient is not connected. Are you running in a context manager?")


class ClickHouseError(Exception):
    """Base Exception representing anything going wrong with ClickHouse."""

    def __init__(self, query, error_message):
        self.query = query
        super().__init__(error_message)


class ClickHouseAllReplicasAreStaleError(ClickHouseError):
    """Exception raised when all replicas are stale."""

    def __init__(self, query, error_message):
        super().__init__(query, error_message)


class ClickHouseClient:
    """An asynchronous client to access ClickHouse via HTTP.

    Attributes:
        session: The underlying aiohttp.ClientSession used for HTTP communication.
        url: The URL of the ClickHouse cluster.
        headers: Headers sent to ClickHouse in an HTTP request. Includes authentication details.
        params: Parameters passed as query arguments in the HTTP request. Common ones include the
            ClickHouse database and the 'max_execution_time'.
    """

    def __init__(
        self,
        url: str = "http://localhost:8123",
        user: str = "default",
        password: str = "",
        database: str = "default",
        timeout: None | aiohttp.ClientTimeout = None,
        ssl: ssl.SSLContext | bool = True,
        **kwargs,
    ):
        self.url = url
        self.headers = {}
        self.params = {}
        self.timeout = timeout
        self.ssl = ssl
        self.connector: None | aiohttp.TCPConnector = None
        self.session: None | aiohttp.ClientSession = None

        logger = get_internal_logger()
        self.logger = logger.bind(url=url, database=database, user=user)

        if user:
            self.headers["X-ClickHouse-User"] = user
        if password:
            self.headers["X-ClickHouse-Key"] = password
        if database:
            self.params["database"] = database

        self.params.update(kwargs)

    @classmethod
    def from_posthog_settings(cls, settings, **kwargs):
        """Initialize a ClickHouseClient from PostHog settings."""
        return cls(
            url=settings.CLICKHOUSE_URL,
            user=settings.CLICKHOUSE_USER,
            password=settings.CLICKHOUSE_PASSWORD,
            database=settings.CLICKHOUSE_DATABASE,
            **kwargs,
        )

    async def is_alive(self) -> bool:
        """Check if the connection is alive by sending a SELECT 1 query.

        Returns:
            A boolean indicating whether the connection is alive.
        """
        if self.session is None:
            raise ClickHouseClientNotConnected()

        ping_url = urljoin(self.url, "ping")

        try:
            await self.session.get(
                url=ping_url,
                headers=self.headers,
                raise_for_status=True,
            )
        except aiohttp.ClientResponseError as exc:
            await self.logger.aexception("Failed ClickHouse liveness check", exc_info=exc)
            return False
        return True

    def prepare_query(self, query: str, query_parameters: None | dict[str, typing.Any] = None) -> str:
        """Prepare the query being sent by encoding and formatting it with the provided parameters.

        Returns:
            The formatted query.
        """
        if not query_parameters:
            return query

        format_parameters = {k: encode_clickhouse_data(v).decode("utf-8") for k, v in query_parameters.items()}
        query = query % format_parameters
        query = query.format(**format_parameters)

        return query

    def prepare_request_data(self, data: collections.abc.Sequence[typing.Any]) -> bytes | None:
        """Prepare the request data sent by encoding it.

        Returns:
            The request data to be passed as the body of the request.
        """
        if len(data) > 0:
            request_data = b",".join(encode_clickhouse_data(value) for value in data)
        else:
            request_data = None
        return request_data

    async def acheck_response(self, response, query) -> None:
        """Asynchronously check the HTTP response received from ClickHouse.

        Raises:
            ClickHouseAllReplicasAreStaleError: If status code is not 200 and error message contains
                "ALL_REPLICAS_ARE_STALE". This can happen when using max_replica_delay_for_distributed_queries
                and fallback_to_stale_replicas_for_distributed_queries=0
            ClickHouseError: If the status code is not 200.
        """
        if response.status != 200:
            error_message = await response.text()
            if "ALL_REPLICAS_ARE_STALE" in error_message:
                raise ClickHouseAllReplicasAreStaleError(query, error_message)
            raise ClickHouseError(query, error_message)

    def check_response(self, response, query) -> None:
        """Check the HTTP response received from ClickHouse.

        Raises:
            ClickHouseAllReplicasAreStaleError: If status code is not 200 and error message contains
                "ALL_REPLICAS_ARE_STALE". This can happen when using max_replica_delay_for_distributed_queries
                and fallback_to_stale_replicas_for_distributed_queries=0
            ClickHouseError: If the status code is not 200.
        """
        if response.status_code != 200:
            error_message = response.text
            if "ALL_REPLICAS_ARE_STALE" in error_message:
                raise ClickHouseAllReplicasAreStaleError(query, error_message)
            raise ClickHouseError(query, error_message)

    @contextlib.asynccontextmanager
    async def aget_query(
        self, query, query_parameters, query_id
    ) -> collections.abc.AsyncIterator[aiohttp.ClientResponse]:
        """Send a GET request to the ClickHouse HTTP interface with a query.

        Only read-only queries may be sent as a GET request. For inserts, use apost_query.

        The context manager protocol is used to control when to release the response.

        Query parameters will be formatted with string formatting and additionally sent to
        ClickHouse in the query string.

        Arguments:
            query: The query to POST.
            *data: Iterable of values to include in the body of the request. For example, the tuples of VALUES for an INSERT query.
            query_parameters: Parameters to be formatted in the query.
            query_id: A query ID to pass to ClickHouse.

        Returns:
            The response received from the ClickHouse HTTP interface.
        """
        if self.session is None:
            raise ClickHouseClientNotConnected()

        params = {**self.params}
        if query_id is not None:
            params["query_id"] = query_id

        # Certain views, like person_batch_exports* still rely on us formatting arguments.
        params["query"] = self.prepare_query(query, query_parameters)

        # TODO: Let clickhouse handle all parameter formatting.
        if query_parameters is not None:
            for key, value in query_parameters.items():
                if key in query:
                    params[f"param_{key}"] = str(value)

        async with self.session.get(url=self.url, headers=self.headers, params=params) as response:
            await self.acheck_response(response, query)
            yield response

    @contextlib.asynccontextmanager
    async def apost_query(
        self, query, *data, query_parameters, query_id
    ) -> collections.abc.AsyncIterator[aiohttp.ClientResponse]:
        """POST a query to the ClickHouse HTTP interface.

        The context manager protocol is used to control when to release the response.

        Query parameters will be formatted with string formatting and additionally sent to
        ClickHouse in the query string.

        Arguments:
            query: The query to POST.
            *data: Iterable of values to include in the body of the request. For example, the tuples of VALUES for an INSERT query.
            query_parameters: Parameters to be formatted in the query.
            query_id: A query ID to pass to ClickHouse.

        Returns:
            The response received from the ClickHouse HTTP interface.
        """
        if self.session is None:
            raise ClickHouseClientNotConnected()

        params = {**self.params}
        if query_id is not None:
            params["query_id"] = query_id

        # Certain views, like person_batch_exports* still rely on us formatting arguments.
        query = self.prepare_query(query, query_parameters)

        # TODO: Let clickhouse handle all parameter formatting.
        if query_parameters is not None:
            for key, value in query_parameters.items():
                if key in query:
                    params[f"param_{key}"] = str(value)

        request_data = self.prepare_request_data(data)

        if request_data:
            params["query"] = query
        else:
            request_data = query.encode("utf-8")

        async with self.session.post(url=self.url, params=params, headers=self.headers, data=request_data) as response:
            await self.acheck_response(response, query)
            yield response

    @contextlib.contextmanager
    def post_query(self, query, *data, query_parameters, query_id) -> collections.abc.Iterator:
        """POST a query to the ClickHouse HTTP interface.

        The context manager protocol is used to control when to release the response.

        Query parameters will be formatted with string formatting and additionally sent to
        ClickHouse in the query string.

        Arguments:
            query: The query to POST.
            *data: Iterable of values to include in the body of the request. For example, the tuples of VALUES for an INSERT query.
            query_parameters: Parameters to be formatted in the query.
            query_id: A query ID to pass to ClickHouse.

        Returns:
            The response received from the ClickHouse HTTP interface.
        """
        params = {**self.params}
        if query_id is not None:
            params["query_id"] = query_id

        query = self.prepare_query(query, query_parameters)
        request_data = self.prepare_request_data(data)

        if request_data:
            params["query"] = query
        else:
            request_data = query.encode("utf-8")

        # TODO: Let clickhouse handle all parameter formatting.
        if query_parameters is not None:
            for key, value in query_parameters.items():
                if key in query:
                    params[f"param_{key}"] = str(value)

        with requests.Session() as s:
            response = s.post(
                url=self.url,
                params=params,
                headers=self.headers,
                data=request_data,
                stream=True,
                verify=False,
            )
            self.check_response(response, query)
            yield response

    async def execute_query(self, query, *data, query_parameters=None, query_id: str | None = None) -> None:
        """Execute the given query in ClickHouse.

        This method doesn't return any response.
        """
        async with self.apost_query(query, *data, query_parameters=query_parameters, query_id=query_id):
            return None

    async def read_query(self, query, query_parameters=None, query_id: str | None = None) -> bytes:
        """Execute the given readonly query in ClickHouse and read the response in full.

        As the entire payload will be read at once, use this method when expecting a small payload, like
        when running a 'count(*)' query.
        """
        async with self.aget_query(query, query_parameters=query_parameters, query_id=query_id) as response:
            return await response.content.read()

    async def stream_query_as_jsonl(
        self,
        query,
        *data,
        query_parameters=None,
        query_id: str | None = None,
        line_separator=b"\n",
    ) -> typing.AsyncGenerator[dict[typing.Any, typing.Any], None]:
        """Execute the given query in ClickHouse and stream back the response as one JSON per line.

        This method makes sense when running with FORMAT JSONEachRow, although we currently do not enforce this.
        """

        buffer = b""
        async with self.apost_query(query, *data, query_parameters=query_parameters, query_id=query_id) as response:
            async for chunk in response.content.iter_any():
                lines = chunk.split(line_separator)

                yield json.loads(buffer + lines[0])

                buffer = lines.pop(-1)

                for line in lines[1:]:
                    yield json.loads(line)

    def stream_query_as_arrow(
        self,
        query,
        *data,
        query_parameters=None,
        query_id: str | None = None,
    ) -> typing.Generator[pa.RecordBatch, None, None]:
        """Execute the given query in ClickHouse and stream back the response as Arrow record batches.

        This method makes sense when running with FORMAT ArrowStreaming, although we currently do not enforce this.
        As pyarrow doesn't support async/await buffers, this method is sync and utilizes requests instead of aiohttp.
        """
        with self.post_query(query, *data, query_parameters=query_parameters, query_id=query_id) as response:
            with pa.ipc.open_stream(pa.PythonFile(response.raw)) as reader:
                yield from reader

    async def astream_query_as_arrow(
        self,
        query,
        *data,
        query_parameters=None,
        query_id: str | None = None,
    ) -> typing.AsyncGenerator[pa.RecordBatch, None]:
        """Execute the given query in ClickHouse and stream back the response as Arrow record batches.

        This method makes sense when running with FORMAT ArrowStream, although we currently do not enforce this.
        """
        async with self.apost_query(query, *data, query_parameters=query_parameters, query_id=query_id) as response:
            reader = asyncpa.AsyncRecordBatchReader(ChunkBytesAsyncStreamIterator(response.content))
            async for batch in reader:
                yield batch

    async def aproduce_query_as_arrow_record_batches(
        self,
        query,
        *data,
        queue: asyncio.Queue,
        query_parameters=None,
        query_id: str | None = None,
    ) -> None:
        """Execute the given query in ClickHouse and produce Arrow record batches to given buffer queue.

        This method makes sense when running with FORMAT ArrowStream, although we currently do not enforce this.
        This method is intended to be ran as a background task, producing record batches continuously, while other
        downstream consumer tasks process them from the queue.
        """
        async with self.apost_query(query, *data, query_parameters=query_parameters, query_id=query_id) as response:
            reader = asyncpa.AsyncRecordBatchProducer(ChunkBytesAsyncStreamIterator(response.content))
            await reader.produce(queue=queue)

    async def __aenter__(self):
        """Enter method part of the AsyncContextManager protocol."""
        self.connector = aiohttp.TCPConnector(ssl=self.ssl)
        self.session = aiohttp.ClientSession(connector=self.connector, timeout=self.timeout)
        return self

    async def __aexit__(self, exc_type, exc_value, tb):
        """Exit method part of the AsyncContextManager protocol."""
        if self.session is not None:
            await self.session.close()

        if self.connector is not None:
            await self.connector.close()

        self.session = None
        self.connector = None
        return False


@contextlib.asynccontextmanager
async def get_client(
    *, team_id: typing.Optional[int] = None, clickhouse_url: str | None = None, **kwargs
) -> collections.abc.AsyncIterator[ClickHouseClient]:
    """
    Returns a ClickHouse client based on the aiochclient library. This is an
    async context manager.

    Usage:

        async with get_client() as client:
            await client.apost_query("SELECT 1")

    Note that this is not a connection pool, so you should not use this for
    queries that are run frequently.

    Note that we setup the SSL context here, allowing for custom CA certs to be
    used. I couldn't see a simply way to do this with `aiochclient` so we
    explicitly use `aiohttp` to create the client session with an ssl_context
    and pass that to `aiochclient`.
    """
    # Set up SSL context, roughly based on how `clickhouse_driver` does it.
    # TODO: figure out why this is not working when we set CERT_REQUIRED. We
    # include a custom CA cert in the Docker image and set the path to it in
    # the settings, but I can't get this to work as expected.
    #
    # ssl_context = ssl.SSLContext(ssl.PROTOCOL_TLS)
    # ssl_context.verify_mode = ssl.CERT_REQUIRED if settings.CLICKHOUSE_VERIFY else ssl.CERT_NONE
    # if ssl_context.verify_mode is ssl.CERT_REQUIRED:
    #    if settings.CLICKHOUSE_CA:
    #        ssl_context.load_verify_locations(settings.CLICKHOUSE_CA)
    #    elif ssl_context.verify_mode is ssl.CERT_REQUIRED:
    #        ssl_context.load_default_certs(ssl.Purpose.SERVER_AUTH)
    timeout = aiohttp.ClientTimeout(total=None, connect=None, sock_connect=30, sock_read=None)

    if team_id is None:
        default_max_block_size = settings.CLICKHOUSE_MAX_BLOCK_SIZE_DEFAULT
    else:
        default_max_block_size = settings.CLICKHOUSE_MAX_BLOCK_SIZE_OVERRIDES.get(
            team_id, settings.CLICKHOUSE_MAX_BLOCK_SIZE_DEFAULT
        )
    max_block_size = kwargs.pop("max_block_size", None) or default_max_block_size

    if clickhouse_url is None:
        url = settings.CLICKHOUSE_OFFLINE_HTTP_URL
    else:
        url = clickhouse_url

    async with ClickHouseClient(
        url=url,
        user=settings.CLICKHOUSE_USER,
        password=settings.CLICKHOUSE_PASSWORD,
        database=settings.CLICKHOUSE_DATABASE,
        timeout=timeout,
        ssl=False,
        max_execution_time=settings.CLICKHOUSE_MAX_EXECUTION_TIME,
        max_memory_usage=settings.CLICKHOUSE_MAX_MEMORY_USAGE,
        max_block_size=max_block_size,
        cancel_http_readonly_queries_on_client_close=1,
        output_format_arrow_string_as_string="true",
        http_send_timeout=0,
        **kwargs,
    ) as client:
        yield client
