#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *initialize*)
      printf '%s
' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"big","version":"1"}}}'
      ;;
    *tools/list*)
      printf '%s
' '[{"name": "alpha1", "description": "capability number 1"}, {"name": "alpha2", "description": "capability number 2"}, {"name": "alpha3", "description": "capability number 3"}, {"name": "alpha4", "description": "capability number 4"}, {"name": "alpha5", "description": "capability number 5"}, {"name": "alpha6", "description": "capability number 6"}, {"name": "alpha7", "description": "capability number 7"}, {"name": "alpha8", "description": "capability number 8"}, {"name": "alpha9", "description": "capability number 9"}, {"name": "alpha10", "description": "capability number 10"}, {"name": "alpha11", "description": "capability number 11"}, {"name": "alpha12", "description": "capability number 12"}, {"name": "alpha13", "description": "capability number 13"}, {"name": "alpha14", "description": "capability number 14"}, {"name": "alpha15", "description": "capability number 15"}, {"name": "alpha16", "description": "capability number 16"}, {"name": "alpha17", "description": "capability number 17"}, {"name": "alpha18", "description": "capability number 18"}, {"name": "alpha19", "description": "capability number 19"}, {"name": "alpha20", "description": "capability number 20"}, {"name": "alpha21", "description": "capability number 21"}]'
      ;;
  esac
done
