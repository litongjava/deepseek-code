#!/bin/sh
# Keep the configured launcher and its MCP child as distinct process witnesses.
export MCP_FIXTURE_OWNER="$PPID"
node "$@"
exit $?
