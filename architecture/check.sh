#!/usr/bin/env bash
set -uo pipefail

die() {
  printf 'architecture: error: %s\n' "$*" >&2
  exit 2
}

((BASH_VERSINFO[0] >= 5)) || die "bash >= 5 required, found $BASH_VERSION"
[ "$#" -eq 1 ] || die "usage: check.sh <workspace-root>"
command -v rg >/dev/null 2>&1 || die "ripgrep (rg) not found on PATH"
[ -d "$1" ] || die "workspace root '$1' is not a directory"
cd -- "$1" || die "cannot enter workspace root '$1'"
[ -f Cargo.toml ] || die "no Cargo.toml at workspace root '$1'"

rg_base=(rg --no-config --hidden --no-ignore)
excludes=(--glob '!/target/' --glob '!/.git/')

collect() {
  local -n into=$1
  shift
  local globs=() glob status
  for glob in "$@"; do
    globs+=(--glob "$glob")
  done
  mapfile -d '' -t into < <("${rg_base[@]}" --files --null "${globs[@]}" "${excludes[@]}")
  wait "$!"
  status=$?
  case $status in
    0) ;;
    1) die "no files matching ${*} under workspace root" ;;
    *) die "listing files matching ${*} failed (rg exit $status)" ;;
  esac
}

collect rust_files '*.rs'
collect manifests 'Cargo.toml' '.cargo/config' '.cargo/config.toml'

violations=0
errors=0

scan() {
  local rule=$1 pattern=$2 status line
  shift 2
  "${rg_base[@]}" --text --line-number --with-filename --color never \
    --regexp "$pattern" -- "$@" |
    while IFS= read -r line; do
      printf '[%s] %s\n' "$rule" "$line"
    done
  status=${PIPESTATUS[0]}
  case $status in
    0) violations=$((violations + 1)) ;;
    1) ;;
    *)
      printf 'architecture: error: rule %s: rg exit %s\n' "$rule" "$status" >&2
      errors=$((errors + 1))
      ;;
  esac
}

scan anyhow '\b(anyhow|eyre)\b' "${rust_files[@]}"
scan dyn '\bdyn\b' "${rust_files[@]}"
scan box 'Box|boxed|\bbox\b' "${rust_files[@]}"
scan async-trait '\b(async_trait|async_recursion)\b' "${rust_files[@]}"
scan error-source '\.(source|sources|cause)\(\s*\)|\bError::(source|cause)\b' "${rust_files[@]}"
scan downcast '\bdowncast\w*' "${rust_files[@]}"
scan code-include '\binclude!|#\s*\[\s*path\s*=' "${rust_files[@]}"
scan lint-override 'disallowed_(types|methods)' "${rust_files[@]}"
scan forbidden-dependency '\b(anyhow|eyre|async[-_]trait|async[-_]recursion)\b' "${manifests[@]}"

printf 'architecture: scanned %d Rust files and %d manifests\n' \
  "${#rust_files[@]}" "${#manifests[@]}" >&2

if ((errors > 0)); then
  die "$errors rule(s) could not be evaluated"
fi
if ((violations > 0)); then
  printf 'architecture: %d rule(s) violated\n' "$violations" >&2
  exit 1
fi
printf 'architecture: ok\n' >&2
