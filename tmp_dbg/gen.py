import json
tools=[{"name":f"alpha{i}","description":f"capability number {i}"} for i in range(1,22)]
tj=json.dumps(tools)
init='{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"big","version":"1"}}}'
script=f'''#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *initialize*)
      printf '%s\n' '{init}'
      ;;
    *tools/list*)
      printf '%s\n' '{tj}'
      ;;
  esac
done
'''
open("tmp_dbg/big-mcp.sh","w").write(script)
