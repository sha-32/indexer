#!/usr/bin/env bash
# Creates and starts the database servers the tests run against, as containers bound to
# localhost only. Run it again to start them after a reboot.
#
#   scripts/test-databases.sh                       # everything but SQL Server
#   ACCEPT_MSSQL_EULA=Y scripts/test-databases.sh   # SQL Server too
#
# The credentials are throwaway values for these local containers. They match the
# connection settings in .cargo/config.toml and are used nowhere else.
#
# Uses podman if it is installed, docker otherwise. On Windows, run it inside WSL:
#   wsl -d Ubuntu -- bash scripts/test-databases.sh
set -u

engine=$(command -v podman || command -v docker) || { echo "podman or docker is needed" >&2; exit 1; }

# up NAME IMAGE [engine arguments...]: starts the container, creating it the first time.
up() {
  local name=$1 image=$2
  shift 2
  if "$engine" container exists "$name" 2>/dev/null || "$engine" inspect "$name" >/dev/null 2>&1; then
    "$engine" start "$name" >/dev/null && echo "started $name"
  else
    "$engine" run -d --name "$name" --restart unless-stopped "$@" "$image" >/dev/null && echo "created $name"
  fi
}

up indexer-postgres docker.io/library/postgres:18 \
  -e POSTGRES_USER=indexer -e POSTGRES_PASSWORD=indexer -e POSTGRES_DB=indexer_test \
  -p 127.0.0.1:5432:5432

up indexer-mysql docker.io/library/mysql:lts \
  -e MYSQL_ROOT_PASSWORD=indexer -p 127.0.0.1:3306:3306

up indexer-mariadb docker.io/library/mariadb:lts \
  -e MARIADB_ROOT_PASSWORD=indexer -p 127.0.0.1:3307:3306

up indexer-clickhouse docker.io/clickhouse/clickhouse-server:latest \
  -e CLICKHOUSE_USER=indexer -e CLICKHOUSE_PASSWORD=indexer -e CLICKHOUSE_DEFAULT_ACCESS_MANAGEMENT=1 \
  -p 127.0.0.1:8123:8123

up indexer-mongodb docker.io/library/mongo:latest -p 127.0.0.1:27017:27017

# Security is off on the two search engines because nothing but the tests can reach them.
up indexer-opensearch docker.io/opensearchproject/opensearch:latest \
  -e discovery.type=single-node -e DISABLE_SECURITY_PLUGIN=true -e DISABLE_INSTALL_DEMO_CONFIG=true \
  -e 'OPENSEARCH_JAVA_OPTS=-Xms512m -Xmx512m' -p 127.0.0.1:9200:9200

up indexer-elasticsearch docker.elastic.co/elasticsearch/elasticsearch:9.1.5 \
  -e discovery.type=single-node -e xpack.security.enabled=false \
  -e 'ES_JAVA_OPTS=-Xms512m -Xmx512m' -p 127.0.0.1:9201:9200

# SQL Server only starts once Microsoft's licence terms are accepted, which is for the
# person running this to do. Developer edition is free for development and testing.
if [ "${ACCEPT_MSSQL_EULA:-}" = "Y" ]; then
  up indexer-mssql mcr.microsoft.com/mssql/server:2022-latest \
    -e ACCEPT_EULA=Y -e MSSQL_PID=Developer -e 'MSSQL_SA_PASSWORD=Indexer!Passw0rd' \
    -p 127.0.0.1:1433:1433
  up indexer-mssql2025 mcr.microsoft.com/mssql/server:2025-latest \
    -e ACCEPT_EULA=Y -e MSSQL_PID=Developer -e 'MSSQL_SA_PASSWORD=Indexer!Passw0rd' \
    -p 127.0.0.1:1434:1433
else
  echo "skipped SQL Server: set ACCEPT_MSSQL_EULA=Y to accept its licence and start it"
fi

"$engine" ps --format '{{.Names}}  {{.Status}}  {{.Ports}}' | grep '^indexer-'
