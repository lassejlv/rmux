#!/usr/bin/env bash
set -euo pipefail

session="rmux-smoke-$$"
single_session="${session}-single"
server_session="${session}-server"
source_session="${session}-source"
break_session="${session}-break"
prefix_session="${session}-prefix"
marker="RMUX_SMOKE_$$_OK"
window_marker="WINDOW_${marker}"
target_marker="TARGET_${marker}"
break_marker="BREAK_$$"
sync_marker="SYNC_${marker}"
source_marker="SRC_$$"
default_marker="DEF_$$"
prefix_marker="^A"
socket="/tmp/rmux-${USER}/${session}.sock"
source_file="/tmp/rmux-source-$$.conf"
invalid_target_log="/tmp/rmux-invalid-target-$$.log"
protected_kill_log="/tmp/rmux-protected-kill-$$.log"
hold_ready_file="/tmp/rmux-hold-ready-$$"

cleanup() {
  ./target/debug/rmux kill-session -t "$session" >/dev/null 2>&1 || true
  ./target/debug/rmux kill-session -t "$single_session" >/dev/null 2>&1 || true
  ./target/debug/rmux kill-session -t "$server_session" >/dev/null 2>&1 || true
  ./target/debug/rmux kill-session -t "$source_session" >/dev/null 2>&1 || true
  ./target/debug/rmux kill-session -t "$break_session" >/dev/null 2>&1 || true
  ./target/debug/rmux kill-session -t "$prefix_session" >/dev/null 2>&1 || true
  rm -f "$socket"
  rm -f "$source_file"
  rm -f "$invalid_target_log"
  rm -f "$protected_kill_log"
  rm -f "$hold_ready_file"
}
trap cleanup EXIT

cargo build >/dev/null

./target/debug/rmux new-session -d -s "$session" -c "cat"

./target/debug/rmux wait-for -t "$session"
./target/debug/rmux new-session -d -A -s "$session" -c "cat"
session_print="$(./target/debug/rmux new-session -d -A -P -F "#{session_name}:#{window_index}.#{pane_index}" -s "$session" -c "cat")"
if [[ "$session_print" != "${session}:1.1" ]]; then
  echo "new-session -P returned unexpected target metadata: $session_print" >&2
  exit 1
fi

./target/debug/rmux new-session -d -s "$prefix_session" -c "cat -v"
./target/debug/rmux set-option -t "$prefix_session" prefix C-a
if [[ "$(./target/debug/rmux display-message -t "$prefix_session" -p "#{prefix_key}")" != "C-a" ]]; then
  echo "set-option did not update prefix key" >&2
  exit 1
fi
./target/debug/rmux bind-key -t "$prefix_session" v split-window --horizontal
if ! ./target/debug/rmux list-keys -t "$prefix_session" | grep -q "^bind-key v split-window --horizontal$"; then
  echo "bind-key was not listed" >&2
  ./target/debug/rmux list-keys -t "$prefix_session" >&2 || true
  exit 1
fi
./target/debug/rmux send-prefix -t "$prefix_session"
./target/debug/rmux wait-for -t "$prefix_session" "$prefix_marker"

./target/debug/rmux new-session -d -s "$source_session" -c "cat"
cat >"$source_file" <<EOF
# exercise tmux-shaped scripted commands
split-window -t "$source_session" --horizontal -c "cat"
set-window-option -t "$source_session" synchronize-panes on
send-keys -t "$source_session" "$source_marker" Enter
set-option -t "$source_session" default-command "printf $default_marker; cat"
split-window -t "$source_session" --horizontal
EOF
./target/debug/rmux source-file "$source_file"
./target/debug/rmux wait-for -t "${source_session}:1.1" "$source_marker"
./target/debug/rmux wait-for -t "${source_session}:1.2" "$source_marker"
./target/debug/rmux wait-for -t "${source_session}:1.3" "$default_marker"
default_state="$(./target/debug/rmux display-message -t "$source_session" -p "#{default_command}")"
if [[ "$default_state" != "printf $default_marker; cat" ]]; then
  echo "set-option did not persist default-command: $default_state" >&2
  exit 1
fi

./target/debug/rmux new-session -d -s "$break_session" -c "cat"
./target/debug/rmux split-window -t "$break_session" --horizontal -c "cat"
./target/debug/rmux send-keys -t "${break_session}:1.2" "$break_marker" Enter
./target/debug/rmux wait-for -t "${break_session}:1.2" "$break_marker"
break_pane_id_before="$(./target/debug/rmux display-message -t "${break_session}:1.2" -p "#{pane_id}")"
break_print="$(./target/debug/rmux break-pane -t "${break_session}:1.2" -n broken -P -F "#{window_name}:#{window_index}:#{pane_id}")"
if [[ "$break_print" != "broken:2:${break_pane_id_before}" ]]; then
  echo "break-pane returned unexpected metadata: $break_print" >&2
  exit 1
fi
./target/debug/rmux wait-for -t "${break_session}:broken.1" "$break_marker"
if [[ "$(./target/debug/rmux display-message -t "${break_session}:broken.1" -p "#{pane_id}:#{pane_count}")" != "${break_pane_id_before}:1" ]]; then
  echo "break-pane did not preserve pane id or isolate pane" >&2
  ./target/debug/rmux display-message -t "${break_session}:broken.1" -p "#{pane_id}:#{pane_count}" >&2 || true
  exit 1
fi
join_print="$(./target/debug/rmux join-pane -s "${break_session}:broken.1" -t "${break_session}:1" -P -F "#{window_index}:#{pane_index}:#{pane_id}:#{window_count}:#{pane_count}")"
if [[ "$join_print" != "1:2:${break_pane_id_before}:1:2" ]]; then
  echo "join-pane returned unexpected metadata: $join_print" >&2
  exit 1
fi
./target/debug/rmux wait-for -t "${break_session}:1.2" "$break_marker"
if ./target/debug/rmux display-message -t "${break_session}:broken.1" -p "#{window_name}" >/dev/null 2>&1; then
  echo "join-pane left the emptied source window behind" >&2
  exit 1
fi

./target/debug/rmux send-keys -t "$session" "$marker" Enter
./target/debug/rmux send-keys -t "$session" --literal "LITERAL_Enter_Tab" Enter
./target/debug/rmux send-keys -t "$session" Enter

rm -f "$hold_ready_file"
./target/debug/rmux hold-client -t "$session" --millis 5000 --ready-file "$hold_ready_file" &
hold_pid=$!
for _ in {1..100}; do
  if [[ -f "$hold_ready_file" ]]; then
    break
  fi
  if ! kill -0 "$hold_pid" 2>/dev/null; then
    wait "$hold_pid" 2>/dev/null || true
    echo "held client exited before becoming ready" >&2
    exit 1
  fi
  sleep 0.05
done
if [[ ! -f "$hold_ready_file" ]]; then
  echo "held client did not become ready" >&2
  kill "$hold_pid" 2>/dev/null || true
  wait "$hold_pid" 2>/dev/null || true
  exit 1
fi
./target/debug/rmux detach-client -t "$session"
for _ in {1..40}; do
  if ! kill -0 "$hold_pid" 2>/dev/null; then
    break
  fi
  sleep 0.05
done
if kill -0 "$hold_pid" 2>/dev/null; then
  echo "detach-client did not detach held client" >&2
  kill "$hold_pid" 2>/dev/null || true
  wait "$hold_pid" 2>/dev/null || true
  exit 1
fi
wait "$hold_pid" 2>/dev/null || true
./target/debug/rmux send-keys -t "$session" "CONCURRENT_${marker}" Enter
split_print="$(./target/debug/rmux split-window -t "$session" --horizontal -P -F "#{pane_index}:#{pane_id}:#{pane_count}" -c "cat")"
if [[ ! "$split_print" =~ ^2:[0-9]+:2$ ]]; then
  echo "split-window -P returned unexpected pane metadata: $split_print" >&2
  exit 1
fi
if ! ./target/debug/rmux list-panes -t "$session" | grep -q "^\\*2:"; then
  echo "new split pane was not active after split" >&2
  ./target/debug/rmux list-panes -t "$session" >&2 || true
  exit 1
fi
./target/debug/rmux previous-pane -t "$session"
if ! ./target/debug/rmux list-panes -t "$session" | grep -q "^\\*1:"; then
  echo "previous-pane did not move to first pane" >&2
  ./target/debug/rmux list-panes -t "$session" >&2 || true
  exit 1
fi
./target/debug/rmux next-pane -t "$session"
if ! ./target/debug/rmux list-panes -t "$session" | grep -q "^\\*2:"; then
  echo "next-pane did not move to second pane" >&2
  ./target/debug/rmux list-panes -t "$session" >&2 || true
  exit 1
fi
./target/debug/rmux select-pane -t "$session" 1
if ! ./target/debug/rmux list-panes -t "$session" | grep -q "^\\*1:"; then
  echo "first pane was not active after select-pane" >&2
  ./target/debug/rmux list-panes -t "$session" >&2 || true
  exit 1
fi
./target/debug/rmux set-window-option -t "$session" synchronize-panes on
sync_state="$(./target/debug/rmux display-message -t "$session" -p "#{synchronize_panes}")"
if [[ "$sync_state" != "1" ]]; then
  echo "set-window-option did not enable synchronize-panes: $sync_state" >&2
  exit 1
fi
./target/debug/rmux send-keys -t "$session" "$sync_marker" Enter
./target/debug/rmux wait-for -t "${session}:1.1" "$sync_marker"
./target/debug/rmux wait-for -t "${session}:1.2" "$sync_marker"
./target/debug/rmux set-window-option -t "$session" synchronize-panes off
sync_state="$(./target/debug/rmux display-message -t "$session" -p "#{synchronize_panes}")"
if [[ "$sync_state" != "0" ]]; then
  echo "set-window-option did not disable synchronize-panes: $sync_state" >&2
  exit 1
fi
./target/debug/rmux select-pane -t "$session" 1
./target/debug/rmux resize-pane -t "$session" -R 5
weights="$(./target/debug/rmux display-message -t "$session" -p "#{pane_weights}")"
if [[ "$weights" != "105,95" ]]; then
  echo "resize-pane did not update pane weights: $weights" >&2
  exit 1
fi
pane_ids_before="$(./target/debug/rmux list-panes -t "$session" -F "#{pane_id}" | paste -sd ':' -)"
./target/debug/rmux swap-pane -t "$session" -D
pane_ids_after="$(./target/debug/rmux list-panes -t "$session" -F "#{pane_id}" | paste -sd ':' -)"
expected_pane_ids_after="$(awk -F: '{ print $2 ":" $1 }' <<<"$pane_ids_before")"
if [[ "$pane_ids_after" != "$expected_pane_ids_after" ]]; then
  echo "swap-pane did not reorder pane ids: before=$pane_ids_before after=$pane_ids_after" >&2
  exit 1
fi
if ! ./target/debug/rmux list-panes -t "$session" -F "#{pane_index}:#{pane_active}" | grep -q "^2:1$"; then
  echo "swap-pane did not keep swapped pane active at its new index" >&2
  ./target/debug/rmux list-panes -t "$session" -F "#{pane_index}:#{pane_active}" >&2 || true
  exit 1
fi
./target/debug/rmux send-keys -t "${session}:1.2" "$target_marker" Enter
./target/debug/rmux wait-for -t "${session}:1.2" "$target_marker"
snapshot="$(./target/debug/rmux capture-pane -t "${session}:1.2")"
if ! grep -q "$target_marker" <<<"$snapshot"; then
  echo "targeted pane output was not captured" >&2
  printf '%s\n' "$snapshot" >&2
  exit 1
fi
if ! ./target/debug/rmux list-panes -t "$session" -F "#{pane_index}:#{pane_active}:#{pane_weight}:#{pane_width}x#{pane_height}" | grep -Eq "^2:1:105:[0-9]+x[0-9]+$"; then
  echo "formatted list-panes did not include second pane" >&2
  ./target/debug/rmux list-panes -t "$session" -F "#{pane_index}:#{pane_active}:#{pane_weight}:#{pane_width}x#{pane_height}" >&2 || true
  exit 1
fi
./target/debug/rmux select-layout -t "$session" even-vertical
layout="$(./target/debug/rmux display-message -t "$session" -p "#{window_layout}")"
if [[ "$layout" != "even-vertical" ]]; then
  echo "select-layout did not switch to even-vertical: $layout" >&2
  exit 1
fi
weights="$(./target/debug/rmux display-message -t "$session" -p "#{pane_weights}")"
if [[ "$weights" != "100,100" ]]; then
  echo "select-layout did not reset pane weights: $weights" >&2
  exit 1
fi
./target/debug/rmux select-layout -t "$session" even-horizontal
layout="$(./target/debug/rmux display-message -t "$session" -p "#{window_layout}")"
if [[ "$layout" != "even-horizontal" ]]; then
  echo "select-layout did not switch to even-horizontal: $layout" >&2
  exit 1
fi
display="$(./target/debug/rmux display-message -t "${session}:1.2" -p "#{session_name}:#{window_index}.#{pane_index}:#{pane_count}")"
if [[ "$display" != "${session}:1.2:2" ]]; then
  echo "display-message returned unexpected target metadata: $display" >&2
  exit 1
fi
if ./target/debug/rmux send-keys -t "${session}:1.99" "SHOULD_NOT_SEND" Enter 2>"$invalid_target_log"; then
  echo "invalid pane target unexpectedly succeeded" >&2
  exit 1
fi
if ! grep -q "pane 99 does not exist" "$invalid_target_log"; then
  echo "invalid pane target did not report a useful error" >&2
  cat "$invalid_target_log" >&2 || true
  exit 1
fi
./target/debug/rmux new-session -d -s "$single_session" -c "cat"
./target/debug/rmux split-window -t "$single_session" -c "cat"
./target/debug/rmux kill-pane -a -t "${single_session}:1.2"
if [[ "$(./target/debug/rmux list-panes -t "$single_session" -F "#{pane_count}:#{pane_active}" | head -n 1)" != "1:1" ]]; then
  echo "kill-pane -a did not leave only the targeted active pane" >&2
  ./target/debug/rmux list-panes -t "$single_session" -F "#{pane_index}:#{pane_count}:#{pane_active}" >&2 || true
  exit 1
fi
if ./target/debug/rmux kill-pane -t "$single_session" 2>"$protected_kill_log"; then
  echo "kill-pane unexpectedly killed the last pane" >&2
  exit 1
fi
if ! grep -q "last pane cannot be killed" "$protected_kill_log"; then
  echo "last pane kill did not report a useful error" >&2
  cat "$protected_kill_log" >&2 || true
  exit 1
fi
if ./target/debug/rmux kill-window -t "$single_session" 2>"$protected_kill_log"; then
  echo "kill-window unexpectedly killed the last window" >&2
  exit 1
fi
if ! grep -q "last window cannot be killed" "$protected_kill_log"; then
  echo "last window kill did not report a useful error" >&2
  cat "$protected_kill_log" >&2 || true
  exit 1
fi
./target/debug/rmux kill-session -t "$single_session"
window_print="$(./target/debug/rmux new-window -t "$session" -n editor -P -F "#{window_index}:#{window_id}:#{pane_id}" -c "printf ${window_marker}; cat")"
if [[ ! "$window_print" =~ ^2:[0-9]+:[0-9]+$ ]]; then
  echo "new-window -P returned unexpected window metadata: $window_print" >&2
  exit 1
fi
./target/debug/rmux wait-for -t "$session" "$window_marker"
snapshot="$(./target/debug/rmux capture-pane -t "$session")"
if ! grep -q "$window_marker" <<<"$snapshot"; then
  echo "new-window command output was not captured" >&2
  printf '%s\n' "$snapshot" >&2
  exit 1
fi
./target/debug/rmux rename-session -t "$session" "renamed-${session}"
if ! ./target/debug/rmux list-sessions -F "#{session_name}:#{window_count}" | grep -q "^renamed-${session}:2$"; then
  echo "formatted list-sessions did not include renamed session metadata" >&2
  ./target/debug/rmux list-sessions -F "#{session_name}:#{window_count}" >&2 || true
  exit 1
fi
if ! ./target/debug/rmux list-windows -t "$session" | grep -q "^\\*2:"; then
  echo "new window was not active after creation" >&2
  ./target/debug/rmux list-windows -t "$session" >&2 || true
  exit 1
fi
if ! ./target/debug/rmux list-windows -t "$session" | grep -q "editor"; then
  echo "renamed window was not listed" >&2
  ./target/debug/rmux list-windows -t "$session" >&2 || true
  exit 1
fi
if ! ./target/debug/rmux list-windows -t "$session" -F "#{window_index}:#{window_name}:#{window_active}" | grep -q "^2:editor:1$"; then
  echo "formatted list-windows did not include editor window" >&2
  ./target/debug/rmux list-windows -t "$session" -F "#{window_index}:#{window_name}:#{window_active}" >&2 || true
  exit 1
fi
./target/debug/rmux swap-window -t "$session" -U
if ! ./target/debug/rmux list-windows -t "$session" -F "#{window_index}:#{window_name}:#{window_active}" | grep -q "^1:editor:1$"; then
  echo "swap-window did not move editor to first active window" >&2
  ./target/debug/rmux list-windows -t "$session" -F "#{window_index}:#{window_name}:#{window_active}" >&2 || true
  exit 1
fi
./target/debug/rmux swap-window -t "$session" -D
if ! ./target/debug/rmux list-windows -t "$session" -F "#{window_index}:#{window_name}:#{window_active}" | grep -q "^2:editor:1$"; then
  echo "swap-window did not move editor back to second active window" >&2
  ./target/debug/rmux list-windows -t "$session" -F "#{window_index}:#{window_name}:#{window_active}" >&2 || true
  exit 1
fi
./target/debug/rmux move-window -t "$session" 1
if ! ./target/debug/rmux list-windows -t "$session" -F "#{window_index}:#{window_name}:#{window_active}" | grep -q "^1:editor:1$"; then
  echo "move-window did not move editor to first active window" >&2
  ./target/debug/rmux list-windows -t "$session" -F "#{window_index}:#{window_name}:#{window_active}" >&2 || true
  exit 1
fi
./target/debug/rmux move-window -t "$session" 2
if ! ./target/debug/rmux list-windows -t "$session" -F "#{window_index}:#{window_name}:#{window_active}" | grep -q "^2:editor:1$"; then
  echo "move-window did not move editor back to second active window" >&2
  ./target/debug/rmux list-windows -t "$session" -F "#{window_index}:#{window_name}:#{window_active}" >&2 || true
  exit 1
fi
./target/debug/rmux select-window -t "$session" 1
./target/debug/rmux last-window -t "$session"
if ! ./target/debug/rmux list-windows -t "$session" -F "#{window_index}:#{window_name}:#{window_active}" | grep -q "^2:editor:1$"; then
  echo "last-window did not return to editor window" >&2
  ./target/debug/rmux list-windows -t "$session" -F "#{window_index}:#{window_name}:#{window_active}" >&2 || true
  exit 1
fi
./target/debug/rmux last-window -t "$session"
if ! ./target/debug/rmux list-windows -t "$session" -F "#{window_index}:#{window_active}" | grep -q "^1:1$"; then
  echo "last-window did not toggle back to first window" >&2
  ./target/debug/rmux list-windows -t "$session" -F "#{window_index}:#{window_active}" >&2 || true
  exit 1
fi
./target/debug/rmux select-window -t "$session" 2
display="$(./target/debug/rmux display-message -t "${session}:editor.1" -p "#{window_name}:#{window_index}.#{pane_index}")"
if [[ "$display" != "editor:2.1" ]]; then
  echo "named window target returned unexpected metadata: $display" >&2
  exit 1
fi
ids="$(./target/debug/rmux display-message -t "${session}:editor.1" -p "#{window_id}:#{pane_id}")"
window_id="${ids%%:*}"
pane_id="${ids##*:}"
display="$(./target/debug/rmux display-message -t "${session}:#${window_id}.%${pane_id}" -p "#{window_name}:#{window_index}.#{pane_index}")"
if [[ "$display" != "editor:2.1" ]]; then
  echo "id target returned unexpected metadata: $display" >&2
  exit 1
fi
respawn_marker="RESPAWN_${marker}"
./target/debug/rmux respawn-pane -t "${session}:editor.1" -c "printf ${respawn_marker}; cat"
./target/debug/rmux wait-for -t "${session}:editor.1" "$respawn_marker"
respawn_pane_id="$(./target/debug/rmux display-message -t "${session}:editor.1" -p "#{pane_id}")"
if [[ "$respawn_pane_id" != "$pane_id" ]]; then
  echo "respawn-pane changed pane id: before=$pane_id after=$respawn_pane_id" >&2
  exit 1
fi
respawn_window_marker="RESPAWN_WINDOW_${marker}"
./target/debug/rmux respawn-window -t "${session}:editor" -c "printf ${respawn_window_marker}; cat"
./target/debug/rmux wait-for -t "${session}:editor.1" "$respawn_window_marker"
respawn_ids="$(./target/debug/rmux display-message -t "${session}:editor.1" -p "#{window_id}:#{pane_id}:#{pane_count}")"
respawn_window_id="${respawn_ids%%:*}"
respawn_rest="${respawn_ids#*:}"
respawn_window_pane_id="${respawn_rest%%:*}"
respawn_window_pane_count="${respawn_rest##*:}"
if [[ "$respawn_window_id" != "$window_id" || "$respawn_window_pane_id" == "$pane_id" || "$respawn_window_pane_count" != "1" ]]; then
  echo "respawn-window returned unexpected ids: before=${window_id}:${pane_id} after=${respawn_ids}" >&2
  exit 1
fi
./target/debug/rmux previous-window -t "$session"
if ! ./target/debug/rmux list-windows -t "$session" | grep -q "^\\*1:"; then
  echo "previous-window did not move to first window" >&2
  ./target/debug/rmux list-windows -t "$session" >&2 || true
  exit 1
fi
./target/debug/rmux next-window -t "$session"
if ! ./target/debug/rmux list-windows -t "$session" | grep -q "^\\*2:"; then
  echo "next-window did not move to second window" >&2
  ./target/debug/rmux list-windows -t "$session" >&2 || true
  exit 1
fi
./target/debug/rmux kill-window -t "$session"
if ./target/debug/rmux list-windows -t "$session" | grep -q "editor"; then
  echo "killed window was still listed" >&2
  ./target/debug/rmux list-windows -t "$session" >&2 || true
  exit 1
fi
./target/debug/rmux select-window -t "$session" 1
./target/debug/rmux select-pane -t "$session" 2

./target/debug/rmux wait-for -t "$session" "$marker"
./target/debug/rmux wait-for -t "$session" "LITERAL_Enter_TabEnter"
./target/debug/rmux wait-for -t "$session" "CONCURRENT_${marker}"
echo "detach/attach smoke ok: $session"
wait "$hold_pid" 2>/dev/null || true
./target/debug/rmux new-session -d -s "$server_session" -c "cat"
./target/debug/rmux wait-for -t "$server_session"
./target/debug/rmux kill-server
for _ in {1..40}; do
  if ! ./target/debug/rmux has-session -t "$session" && ! ./target/debug/rmux has-session -t "$server_session"; then
    exit 0
  fi
  sleep 0.05
done
if ./target/debug/rmux has-session -t "$session" || ./target/debug/rmux has-session -t "$server_session"; then
  echo "kill-server left a session running" >&2
  ./target/debug/rmux list-sessions >&2 || true
  exit 1
fi

exit 0
