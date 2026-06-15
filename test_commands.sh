#!/usr/bin/env bash
# Handy demo commands for manual demons testing.

# cpu-seconds: prints CPU core count and current temp (if sensors available) every second.
cpu_seconds() {
    while true; do
        printf '[%s] cores: %s  temp: ' "$(date +%H:%M:%S)" "$(nproc)"
        if command -v sensors >/dev/null 2>&1; then
            sensors 2>/dev/null | awk '/Tccd1|Tdie|Tctl|TCPU/ {printf "%s ", $2; found=1} END {if(!found) printf "N/A"}'
        else
            printf 'N/A'
        fi
        printf '\n'
        sleep 1
    done
}

# clock: prints the time every second with a rotating spinner.
clock() {
    local chars='|/-\\'
    local i=0
    while true; do
        local c="${chars:i%4:1}"
        printf '\r\033[K[%s] clock %s' "$(date +%H:%M:%S)" "$c"
        sleep 1
        i=$((i + 1))
    done
}

# load: prints 1/5/15 load average every 2 seconds.
load() {
    while true; do
        awk '{printf "[%s] load: %s %s %s\n", strftime("%H:%M:%S"), $1, $2, $3}' /proc/loadavg
        sleep 2
    done
}

# colored-count: prints a colored, incrementing counter every half second.
colored_count() {
    local n=0
    while true; do
        color=$((31 + (n % 7)))
        printf '\033[%dmcount: %d\033[0m\n' "$color" "$n"
        n=$((n + 1))
        sleep 0.5
    done
}

case "${1:-}" in
    cpu) cpu_seconds ;;
    clock) clock ;;
    load) load ;;
    count) colored_count ;;
    *) echo "usage: $0 {cpu|clock|load|count}" ;;
esac
