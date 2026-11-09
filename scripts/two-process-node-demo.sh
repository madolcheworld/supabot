set -uo pipefail
cd /home/riyan/Dokumen/bot/prototype
BIN=target/debug
WORK=$(pwd)/target/two-process/run
rm -rf "$WORK"; mkdir -p "$WORK/brain" "$WORK/node"
BRAIN_DIR=$WORK/brain; NODE_DIR=$WORK/node
PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')
URL=http://127.0.0.1:$PORT
echo "### brain on $URL, data in $WORK"

RUST_LOG=info $BIN/swarmbot-brain --data-dir "$BRAIN_DIR" --addr 127.0.0.1:$PORT \
  --internal-addr "" --offline --team config/team.example.toml > $WORK/brain.log 2>&1 &
BRAIN_PID=$!
TOKEN=""
for i in $(seq 1 300); do
  [ -f "$BRAIN_DIR/owner-token" ] && TOKEN=$(cat $BRAIN_DIR/owner-token)
  [ -n "$TOKEN" ] && curl -sf -H "Authorization: Bearer $TOKEN" $URL/api/health >/dev/null 2>&1 && break
  sleep 0.1
done
echo "### health: $(curl -s -H "Authorization: Bearer $TOKEN" $URL/api/health)"
echo
echo "### 1. the boot sequence, as logged"
grep -E 'swarmbot-brain starting|configuration resolved|secrets loaded|brain state loaded|the roster is ready|owner token ready|node identity|public listener is up' $WORK/brain.log | head -10
echo
echo "### the one-time login URL on stdout (token shown redacted here):"
sed -n '/Open this URL once/,+3p' $WORK/brain.log | sed "s/$TOKEN/<owner-token>/"
echo
echo "### 2. swarmbot nodes pair  (the CLI, against this brain's API)"
$BIN/swarmbot --brain $URL --token "$TOKEN" nodes pair 2>&1 | tee $WORK/pair.txt
CODE=$(grep -oE '[0-9A-Z]{4}-[0-9A-Z]{4}' $WORK/pair.txt | head -1)
echo
echo "### 3. swarmbot-node pair $URL $CODE"
$BIN/swarmbot-node --data-dir "$NODE_DIR" pair $URL "$CODE" 2>&1 | tail -6
echo "### identity.json (mode $(stat -c %a $NODE_DIR/identity.json 2>/dev/null || echo '?')):"
python3 -c "
import json
d=json.load(open('$NODE_DIR/identity.json'))
print({k:(v[:14]+'...' if isinstance(v,str) and len(v)>20 else v) for k,v in d.items()})"
echo
echo "### 4. swarmbot-node run --label demo-node --ceiling shell=ask --ceiling read_file=always"
RUST_LOG=info $BIN/swarmbot-node --data-dir "$NODE_DIR" run --label demo-node \
  --ceiling shell=ask --ceiling read_file=always > $WORK/node.log 2>&1 &
NODE_PID=$!
for i in $(seq 1 300); do grep -qiE 'hello|authenticated|serving' $WORK/node.log 2>/dev/null && break; sleep 0.1; done
sleep 1.5
echo "--- node.log:"; cat $WORK/node.log | head -12
echo "--- brain.log (the same handshake, other side):"
grep -E 'upgrade signature verified|a node is connected|authenticated and live|announced itself' $WORK/brain.log | head -5
echo
echo "### 5. swarmbot nodes ls"
$BIN/swarmbot --brain $URL --token "$TOKEN" nodes ls 2>&1 | head -10
NODE_ID=$(curl -s -H "Authorization: Bearer $TOKEN" $URL/api/nodes | python3 -c "
import json,sys
print([n['id'] for n in json.load(sys.stdin) if n['kind']=='remote'][0])")
echo
echo "### GET /api/nodes — the remote row the hello produced:"
curl -s -H "Authorization: Bearer $TOKEN" $URL/api/nodes | python3 -c "
import json,sys
n=[x for x in json.load(sys.stdin) if x['kind']=='remote'][0]
n['policy']={'default':n['policy']['default'],'surfaces':n['policy']['surfaces']}
print(json.dumps(n,indent=1))"
echo
echo "### swarmbot-node status"
$BIN/swarmbot-node --data-dir "$NODE_DIR" status 2>&1 | head -18
echo "### 6. heartbeats (every 15 s) keep last_seen_at honest — waiting 17 s"
BEFORE=$(curl -s -H "Authorization: Bearer $TOKEN" $URL/api/nodes | python3 -c "
import json,sys
print([x for x in json.load(sys.stdin) if x['id']=='$NODE_ID'][0]['last_seen_at'])")
sleep 17
AFTER=$(curl -s -H "Authorization: Bearer $TOKEN" $URL/api/nodes | python3 -c "
import json,sys
n=[x for x in json.load(sys.stdin) if x['id']=='$NODE_ID'][0]
print(n['last_seen_at'])")
echo "last_seen_at before: $BEFORE"
echo "last_seen_at after:  $AFTER"
[ "$BEFORE" != "$AFTER" ] && echo "### a heartbeat moved it" || echo "### !!! last_seen_at did not move"
echo "--- the brain saw the heartbeats:"
grep -c 'heartbeat' $WORK/brain.log | sed 's/^/heartbeat log lines: /' 
echo
echo "### 7. revoke: DELETE /api/nodes/$NODE_ID"
curl -s -o /dev/null -w 'http status = %{http_code}\n' -X DELETE -H "Authorization: Bearer $TOKEN" $URL/api/nodes/$NODE_ID
for i in $(seq 1 100); do kill -0 $NODE_PID 2>/dev/null || break; sleep 0.1; done
echo "--- node.log after the revoke:"
tail -6 $WORK/node.log
if kill -0 $NODE_PID 2>/dev/null; then echo "!!! the node is STILL RUNNING"; kill $NODE_PID 2>/dev/null; else wait $NODE_PID; echo "### the node exited by itself, code $?"; fi
echo
echo "### the roster row after the revoke:"
curl -s -H "Authorization: Bearer $TOKEN" $URL/api/nodes | python3 -c "
import json,sys
n=[x for x in json.load(sys.stdin) if x['id']=='$NODE_ID'][0]
print({'online':n['online'],'revoked':n['revoked'],'declared':n['declared'],'approved':n['approved']})"
echo
echo "### 8. the revoked node redials and is refused before any upgrade:"
timeout 15 $BIN/swarmbot-node --data-dir "$NODE_DIR" run --label demo-node 2>&1 | grep -iE 'refus|401|unreachable|revok|error' | head -4
echo
echo "### 9. POST /api/shutdown"
curl -s -X POST -H "Authorization: Bearer $TOKEN" $URL/api/shutdown; echo
wait $BRAIN_PID; echo "### the brain exited with code $?"
tail -3 $WORK/brain.log
