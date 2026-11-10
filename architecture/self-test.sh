#!/usr/bin/env bash
set -uo pipefail

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd) || exit 2
checker=$here/check.sh
fixtures=$here/fixtures
work=$(mktemp -d) || exit 2
trap 'chmod -R u+rwx "$work" 2>/dev/null; rm -rf "$work"' EXIT

passed=0
failed=0
case_id=0
tree=

pass() {
  passed=$((passed + 1))
}

fail() {
  printf 'FAIL %s\n' "$1" >&2
  [ -n "${2:-}" ] && printf '%s\n' "$2" | sed 's/^/    /' >&2
  failed=$((failed + 1))
}

new_tree() {
  case_id=$((case_id + 1))
  tree=$work/case-$case_id
  [ ! -e "$tree" ] || exit 2
  mkdir -p "$tree/crates/m/src" || exit 2
  place Cargo.toml "$fixtures/allowed/Cargo.toml.txt"
  place crates/m/Cargo.toml "$fixtures/allowed/Cargo.toml.txt"
  place crates/m/src/lib.rs "$fixtures/allowed/lib.rs.txt"
}

place() {
  mkdir -p "$(dirname -- "$tree/$1")" || exit 2
  rm -f "$tree/$1" || exit 2
  cp "$2" "$tree/$1" || exit 2
  chmod u+w "$tree/$1" || exit 2
}

expect() {
  local name=$1 want=$2 root=$3 needle=${4:-} output status
  output=$(bash "$checker" "$root" 2>&1)
  status=$?
  if [ "$status" -ne "$want" ]; then
    fail "$name: want exit $want, got $status" "$output"
  elif [ -n "$needle" ] && [[ $output != *"$needle"* ]]; then
    fail "$name: output lacks '$needle'" "$output"
  else
    pass
  fi
}

new_tree
expect allowed 0 "$tree"

new_tree
place target/debug/build/x/out/generated.rs "$fixtures/forbidden-source/dyn--arc.rs.txt"
place .git/hooks/x.rs "$fixtures/forbidden-source/dyn--arc.rs.txt"
expect build-output-skipped 0 "$tree"

locations=(
  crates/m/src/lib.rs
  crates/m/src/target/mod.rs
  crates/m/tests/integration.rs
  crates/m/examples/example.rs
  crates/m/benches/bench.rs
  crates/m/build.rs
  build.rs
  .hidden/module.rs
)
for fixture in "$fixtures"/forbidden-source/*.rs.txt; do
  base=$(basename -- "$fixture" .rs.txt)
  rule=${base%%--*}
  for location in "${locations[@]}"; do
    new_tree
    place "$location" "$fixture"
    expect "source $base at $location" 1 "$tree" "[$rule] $location:"
  done
done

for fixture in "$fixtures"/forbidden-manifest/*.toml.txt; do
  base=$(basename -- "$fixture" .toml.txt)
  for location in Cargo.toml crates/m/Cargo.toml; do
    new_tree
    place "$location" "$fixture"
    expect "manifest $base at $location" 1 "$tree" "[forbidden-dependency] $location:"
  done
done

expect error-missing-root 2 "$work/does-not-exist"

new_tree
rm -r "$tree/crates"
expect error-no-rust-files 2 "$tree"

new_tree
rm "$tree/Cargo.toml"
expect error-no-root-manifest 2 "$tree"

new_tree
place crates/m/src/hidden.rs "$fixtures/forbidden-source/dyn--arc.rs.txt"
chmod 000 "$tree/crates/m/src/hidden.rs"
if [ -r "$tree/crates/m/src/hidden.rs" ]; then
  fail "error-unreadable-file: cannot make a file unreadable (running as root?)"
else
  expect error-unreadable-file 2 "$tree"
fi

new_tree
place crates/m/src/sealed/mod.rs "$fixtures/allowed/lib.rs.txt"
chmod 000 "$tree/crates/m/src/sealed"
if [ -r "$tree/crates/m/src/sealed" ]; then
  fail "error-unreadable-directory: cannot make a directory unreadable (running as root?)"
else
  expect error-unreadable-directory 2 "$tree"
fi

new_tree
output=$(PATH=/nonexistent "$BASH" "$checker" "$tree" 2>&1)
status=$?
if [ "$status" -eq 2 ]; then
  pass
else
  fail "error-missing-ripgrep: want exit 2, got $status" "$output"
fi

printf 'architecture self-test: %d passed, %d failed\n' "$passed" "$failed" >&2
[ "$failed" -eq 0 ]
