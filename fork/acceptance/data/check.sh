#!/usr/bin/env bash
# The RDS instance is a real PostgreSQL server: a client that reads the master password
# from the secret RDS manages logs in over TLS on the published endpoint, gets the major
# version the instance was created with, and installs an extension the emulator's
# in-process engine does not have. The Valkey replication group is a real Valkey server too: a
# value written over TLS on its primary endpoint is read back. run.sh starts the emulator with
# EMULATOR_RDS_BACKEND=docker and EMULATOR_ELASTICACHE_BACKEND=docker.
set -euo pipefail

endpoint=$(terraform output -raw db_endpoint)
host=${endpoint%:*}
port=${endpoint##*:}
secret=$(aws rds describe-db-instances --db-instance-identifier acceptance \
  --query 'DBInstances[0].MasterUserSecret.SecretArn' --output text)
password=$(aws secretsmanager get-secret-value --secret-id "$secret" --query SecretString --output text | jq -r .password)

psql() {
  docker run --rm --network host -e PGPASSWORD="$password" postgres:17 \
    psql "host=$host port=$port user=app dbname=app sslmode=require connect_timeout=30" -v ON_ERROR_STOP=1 -tAc "$1"
}

version=$(psql 'SELECT version()')
if [[ $version != "PostgreSQL 17"* ]]; then
  echo "check: the instance answers version() with '$version', not PostgreSQL 17" >&2
  exit 1
fi
psql 'CREATE EXTENSION IF NOT EXISTS pgcrypto' >/dev/null
digest=$(psql "SELECT encode(digest('abc', 'sha256'), 'hex')")
if [[ $digest != ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad ]]; then
  echo "check: pgcrypto's digest('abc') is '$digest'" >&2
  exit 1
fi
echo "check: $endpoint is ${version%% on *} with pgcrypto, logged in with the managed secret over TLS"

read -r cache_host cache_port < <(aws elasticache describe-replication-groups --replication-group-id acceptance \
  --query 'ReplicationGroups[0].NodeGroups[0].PrimaryEndpoint.[Address,Port]' --output text)
valkey() {
  docker run --rm --network host valkey/valkey:8 valkey-cli -h "$cache_host" -p "$cache_port" --tls --insecure "$@"
}
stored=""
for _ in $(seq 60); do
  stored=$(valkey SET acceptance-key acceptance-value 2>&1 || true)
  [[ $stored == OK ]] && break
  sleep 5
done
if [[ $stored != OK ]]; then
  echo "check: SET on $cache_host:$cache_port answered '$stored'" >&2
  exit 1
fi
read_back=$(valkey GET acceptance-key)
if [[ $read_back != acceptance-value ]]; then
  echo "check: GET on $cache_host:$cache_port answered '$read_back'" >&2
  exit 1
fi
echo "check: $cache_host:$cache_port is a Valkey server over TLS (SET and GET)"
