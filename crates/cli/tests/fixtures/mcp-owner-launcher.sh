#!/bin/sh
# Keep the configured launcher and its MCP child as distinct process witnesses.
export MCP_FIXTURE_OWNER="$PPID"
if [ "${MCP_FIXTURE_SETSID:-}" = 1 ]; then
    setsid node "$@"
else
    node "$@"
fi
exit $?
