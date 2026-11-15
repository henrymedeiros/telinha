#!/bin/sh
# Runs a command in a network namespace of its own whose loopback goes through
# netem, without root: a capped or jittery network for measure.mjs. Both
# browsers, the local signaling server and the desktop app all talk over
# 127.0.0.1 in there, so everything is shaped, in both directions together.
#
#   unshare -rn sh tools/latency/netem-netns.sh "<netem options>" <command...>
#
# Nothing in the namespace reaches the internet: use --sinal-local.
#
# From tools/latency, after `npm install`:
#   a link slower than the stream
#     unshare -rn sh netem-netns.sh "rate 3mbit limit 200" \
#       node measure.mjs --app ../.. --sinal-local --turbo --canvas
#   plenty of bandwidth, bursty delivery (like a busy Wi-Fi), the desktop app streaming
#     unshare -rn sh netem-netns.sh "rate 40mbit delay 4ms 30ms limit 1000" \
#       env LD_LIBRARY_PATH="$PWD/../../native/.deps/libva/lib:$PWD/../../native/.deps/ffmpeg-linux/lib" \
#       NATIVO_LOG=/tmp/nativo.log \
#       node measure.mjs --app ../.. --sinal-local --nativo ../../native/target/debug/telinha --turbo --canvas
# NATIVO_LOG keeps the app's log: its "viewer" and "adjusting bitrate" lines say how it coped.
set -eu
[ "$#" -ge 2 ] || { echo "usage: unshare -rn sh $0 \"<netem options>\" <command...>" >&2; exit 2; }
NETEM="$1"; shift
[ "$(id -u)" = 0 ] || { echo "run it as: unshare -rn sh $0 ..." >&2; exit 2; }
# Who we were before `unshare -r` made this shell root in its own user namespace.
OUT_UID=$(awk 'NR==1 {print $2}' /proc/self/uid_map)
OUT_GID=$(awk 'NR==1 {print $2}' /proc/self/gid_map)
ip link set lo up
# webrtc-rs and Chrome both need more than a loopback to start: an interface
# with an address and a default route, which lead nowhere.
ip link add tl0 type dummy
ip addr add 10.77.0.1/24 dev tl0
ip link set tl0 up
ip route add default dev tl0
# Unquoted on purpose: the options are several words.
# shellcheck disable=SC2086
tc qdisc add dev lo root netem $NETEM
# Chrome refuses to run as root without --no-sandbox: back to the ordinary
# user, still inside the shaped namespace.
exec unshare -U --map-user="$OUT_UID" --map-group="$OUT_GID" "$@"
