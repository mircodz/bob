# /// script
# requires-python = ">=3.12"
# dependencies = [
#   "mcp==2.3.0",
#   "azure-kusto-data==6.0.4",
#   "azure-identity==1.27.0",
# ]
# ///
"""Kusto (Azure Data Explorer) MCP server for bob.

Run with `uv run mcp/kusto/server.py`. Clusters are configured by name in ~/.bob/kusto.toml:

    [clusters.prod]
    url = "https://mycluster.westeurope.kusto.windows.net"
    default_database = "Telemetry"

Queries are read-only unless `allow_commands = true` is set for a cluster.
Sign-in uses a cached interactive browser login.
"""

import os
import tomllib
from pathlib import Path

from azure.identity import InteractiveBrowserCredential, TokenCachePersistenceOptions
from azure.kusto.data import ClientRequestProperties, KustoClient, KustoConnectionStringBuilder
from azure.kusto.data.exceptions import KustoError
from mcp.server.mcpserver import MCPServer
from mcp.server.mcpserver.exceptions import ToolError

CONFIG_PATH = Path(os.environ.get("KUSTO_MCP_CONFIG", Path.home() / ".bob" / "kusto.toml"))
MAX_ROWS = 200
MAX_CELL_CHARS = 500

server = MCPServer(
    name="kusto",
    instructions=(
        "Query Azure Data Explorer (Kusto). Call list_clusters first, then "
        "table_schema before writing KQL against an unfamiliar table."
    ),
)

_credential = InteractiveBrowserCredential(
    cache_persistence_options=TokenCachePersistenceOptions(name="bob-kusto-mcp")
)
_clients: dict[str, KustoClient] = {}


def _config() -> dict:
    if not CONFIG_PATH.exists():
        raise ToolError(f"No Kusto config found at {CONFIG_PATH}; add a [clusters.<name>] entry.")
    return tomllib.loads(CONFIG_PATH.read_text()).get("clusters", {})


def _cluster(name: str) -> dict:
    clusters = _config()
    if name not in clusters:
        known = ", ".join(sorted(clusters)) or "none"
        raise ToolError(f"Unknown cluster '{name}'. Configured clusters: {known}.")
    return clusters[name]


def _client(name: str) -> KustoClient:
    if name not in _clients:
        url = _cluster(name)["url"]
        builder = KustoConnectionStringBuilder.with_azure_token_credential(url, _credential)
        _clients[name] = KustoClient(builder)
    return _clients[name]


def _database(cluster: str, database: str | None) -> str:
    database = database or _cluster(cluster).get("default_database")
    if not database:
        raise ToolError(f"No database given and cluster '{cluster}' has no default_database.")
    return database


def _cell(value: object) -> str:
    text = "" if value is None else str(value).replace("\n", " ")
    return text if len(text) <= MAX_CELL_CHARS else text[: MAX_CELL_CHARS - 1] + "…"


def _run(cluster: str, database: str, text: str, properties=None, command: bool = False):
    client = _client(cluster)
    try:
        if command:
            return client.execute_mgmt(database, text, properties)
        return client.execute(database, text, properties)
    except KustoError as error:
        raise ToolError(f"Kusto error: {error}") from error


def _table(result) -> str:
    table = result.primary_results[0]
    columns = [column.column_name for column in table.columns]
    rows = [[_cell(value) for value in row] for row in table]
    lines = [" | ".join(columns), " | ".join("---" for _ in columns)]
    lines += [" | ".join(row) for row in rows[:MAX_ROWS]]
    if len(rows) > MAX_ROWS:
        lines.append(
            f"({len(rows)} rows total; showing the first {MAX_ROWS}. "
            "Aggregate or add `| take` / filters to narrow the result.)"
        )
    elif not rows:
        lines.append("(no rows)")
    return "\n".join(lines)


@server.tool()
def list_clusters() -> str:
    """List the configured Kusto clusters and their default databases."""
    clusters = _config()
    if not clusters:
        return f"No clusters configured in {CONFIG_PATH}."
    return "\n".join(
        f"{name}: {entry['url']} (default database: {entry.get('default_database', 'none')})"
        for name, entry in sorted(clusters.items())
    )


@server.tool()
def list_databases(cluster: str) -> str:
    """List databases in a configured cluster."""
    return _table(_run(cluster, "", ".show databases | project DatabaseName", command=True))


@server.tool()
def list_tables(cluster: str, database: str | None = None) -> str:
    """List tables in a database (defaults to the cluster's default_database)."""
    database = _database(cluster, database)
    return _table(_run(cluster, database, ".show tables | project TableName", command=True))


@server.tool()
def table_schema(cluster: str, table: str, database: str | None = None) -> str:
    """Show column names and types for a table. Use before writing KQL against it."""
    database = _database(cluster, database)
    command = f".show table ['{table}'] schema as csl"
    return _table(_run(cluster, database, command, command=True))


@server.tool()
def query(cluster: str, kql: str, database: str | None = None) -> str:
    """Run a KQL query. Control commands (starting with '.') are rejected unless enabled."""
    database = _database(cluster, database)
    if kql.lstrip().startswith(".") and not _cluster(cluster).get("allow_commands", False):
        raise ToolError(
            "Control commands are disabled for this cluster; set allow_commands = true to enable."
        )
    properties = ClientRequestProperties()
    properties.set_option(ClientRequestProperties.request_readonly_option_name, True)
    return _table(_run(cluster, database, kql, properties))


if __name__ == "__main__":
    server.run("stdio")
