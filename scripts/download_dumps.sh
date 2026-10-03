#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# download_dumps.sh — MusicBrainz fullexport downloader with checksum gate
#
# Fetches the latest MusicBrainz fullexport archives (core + derived) and —
# with --sample — the monthly sample dump used for the P0 smoke import,
# verifying every archive against the shipped MD5SUMS before it is offered
# to `mbdb import`.  Both data profiles publish a `LATEST` marker file that
# names the current dated release directory, which the script resolves
# before downloading.  Mirrors the wiki_db download_dump.sh house pattern
# (resumable curl, descriptive User-Agent, checksum-first gate).
#
# Usage:
#   ./scripts/download_dumps.sh [--dump-dir <DIR>] [--core-only]
#                               [--sample] [--force] [--quiet] [--help]
#
# Options:
#   --dump-dir <DIR>  Target dump directory
#                     (default: ~/musicbrainz_db/dumps — matches config.toml)
#   --core-only       Skip the derived archive (core load-target only)
#   --sample          Also fetch the monthly sample dump (P0 smoke fixture)
#   --sample-only     Fetch only the monthly sample dump (P0 smoke fixture)
#   --force           Re-download archives even when a valid copy exists
#   --quiet           Suppress progress output
#   --help            Show this help text and exit
#
# Resume behaviour:
#   Interrupted downloads are left in place (curl -C - resumes them on the
#   next run).  A weekly re-run detects a refreshed fullexport by fetching
#   the new MD5SUMS first: any existing archive whose checksum no longer
#   matches is re-downloaded.  A corrupt download is removed and the run
#   aborts with a non-zero exit — never import an unverified archive.
# ---------------------------------------------------------------------------
set -euo pipefail

# ---------- Configuration ------------------------------------------------
BASE_URL="https://data.metabrainz.org/pub/musicbrainz/data"
DUMP_DIR="$HOME/musicbrainz_db/dumps"
USER_AGENT="music_db_mb/0.1.0 (musicbrainz-mbdb fullexport downloader; local mbdb project)"
CORE=true
DERIVED=true
SAMPLE=false
FORCE=false
QUIET=false

# ---------- Argument parsing ---------------------------------------------
usage() { sed -n '3,32p' "$0" | sed 's/^# //; s/^#$//'; }

while [[ $# -gt 0 ]]; do
	case "$1" in
	--dump-dir)
		shift
		DUMP_DIR="$1"
		;;
	--core-only)
		DERIVED=false
		;;
	--sample)
		SAMPLE=true
		;;
	--sample-only)
		CORE=false
		DERIVED=false
		SAMPLE=true
		;;
	--force)
		FORCE=true
		;;
	--quiet)
		QUIET=true
		;;
	--help | -h)
		usage
		exit 0
		;;
	*)
		echo "Unknown option: $1" >&2
		echo "Usage: $0 [--dump-dir <DIR>] [--core-only] [--sample] [--force] [--quiet]" >&2
		exit 1
		;;
	esac
	shift
done

# ---------- Helpers ------------------------------------------------------
info() { [[ "$QUIET" == false ]] && echo "[INFO]  $*"; }
warn() { echo "[WARN]  $*" >&2; }
error() { echo "[ERROR] $*" >&2; }

# ---------- Signal handling ----------------------------------------------
trap 'info "Interrupted — partial downloads left in place for resume."; exit 1' SIGINT SIGTERM

# ---------- Pre-flight checks --------------------------------------------
for cmd in curl md5sum; do
	if ! command -v "$cmd" &>/dev/null; then
		error "Required command not found: $cmd"
		exit 1
	fi
done
mkdir -p "$DUMP_DIR"

# ---------- Checksum verification -----------------------------------------
verify_one() {
	local file="$1" sums="$2"
	[[ -f "$file" ]] || { error "missing archive: $file"; return 1; }
	local expected actual
	expected=$(awk -v f="$(basename "$file")" '
		{ for (i = 2; i <= NF; i++) if ($i == f || $i == "./" f || $i == "*" f) { print $1; exit } }
	' "$sums")
	if [[ -z "$expected" ]]; then
		warn "no checksum entry for $(basename "$file") in $(basename "$sums") — skipping verification"
		return 0
	fi
	actual=$(md5sum "$file" | awk '{print $1}')
	if [[ "$actual" != "$expected" ]]; then
		error "MD5 MISMATCH for $(basename "$file")"
		error "  expected: $expected"
		error "  got:      $actual"
		return 1
	fi
	info "MD5 OK: $(basename "$file") ($actual)"
}

# ---------- Download loop --------------------------------------------------
download_one() {
	local url="$1" file="$2" sums="$3"
	if [[ -f "$file" && "$FORCE" == false ]]; then
		if bzip2 -t "$file" 2>/dev/null; then
			if verify_one "$file" "$sums"; then
				info "already present and verified — skipping: $(basename "$file")"
				return 0
			fi
			warn "existing $(basename "$file") failed checksum (refreshed release?) — fetching a fresh copy"
			rm -f "$file"
		else
			warn "existing $(basename "$file") is not a complete bzip2 stream — resuming"
		fi
	fi
	local progress="--progress-bar"
	[[ "$QUIET" == true ]] && progress="--silent"
	info "Downloading $(basename "$file")…"
	curl -C - -o "$file" -A "$USER_AGENT" -L --retry 3 --retry-delay 5 "$progress" "$url" || {
		error "download failed (curl exit $?) — partial file left for resume"
		exit 1
	}
	verify_one "$file" "$sums" || {
		rm -f "$file"
		error "corrupt download removed. Re-run to retry."
		exit 1
	}
}

# ---------- Fetch checksums -----------------------------------------------
# Both `fullexport` and `sample` publish a `LATEST` *file* (not a symlink)
# containing the dated directory name for the current release, e.g.
# `fullexport/20260930-002222/`.  Resolve it first, then verify against the
# checked archive directory's own MD5SUMS.
resolve_latest() {
	local profile="$1"
	curl -sS -A "$USER_AGENT" -L --retry 3 --retry-delay 5 "$BASE_URL/$profile/LATEST" | tr -d '[:space:]'
}

FULL_DATE=$(resolve_latest fullexport) || {
	error "could not resolve fullexport LATEST — refusing to download unverified archives"
	exit 1
}
[[ -n "$FULL_DATE" ]] || {
	error "fullexport LATEST resolved to an empty name"
	exit 1
}
FULL_URL="$BASE_URL/fullexport/$FULL_DATE"
FULL_SUMS="$DUMP_DIR/mbdump.MD5SUMS"
info "Fetching fullexport checksums ($FULL_URL/MD5SUMS)…"
if ! curl -sS -o "$FULL_SUMS" -A "$USER_AGENT" -L --retry 3 --retry-delay 5 "$FULL_URL/MD5SUMS"; then
	error "could not fetch fullexport MD5SUMS — refusing to download unverified archives"
	exit 1
fi

# ---------- Download archives ---------------------------------------------
if [[ "$CORE" == true ]]; then
	download_one "$FULL_URL/mbdump.tar.bz2" \
		"$DUMP_DIR/mbdump.tar.bz2" "$FULL_SUMS"
fi
if [[ "$DERIVED" == true ]]; then
	download_one "$FULL_URL/mbdump-derived.tar.bz2" \
		"$DUMP_DIR/mbdump-derived.tar.bz2" "$FULL_SUMS"
fi
if [[ "$SAMPLE" == true ]]; then
	SAMPLE_DATE=$(resolve_latest sample) || true
	[[ -n "$SAMPLE_DATE" ]] || {
		error "could not resolve sample LATEST"
		exit 1
	}
	# The sample directory ships no MD5SUMS (archive + .asc only), so
	# verification is an `xz -t` integrity test after download instead.
	warn "sample dir ships no MD5SUMS — verifying archive integrity with xz -t (scratch DB only)"
	SAMPLE_FILE="$DUMP_DIR/mbdump-sample.tar.xz"
	if [[ -f "$SAMPLE_FILE" && "$FORCE" == false ]]; then
		if xz -t "$SAMPLE_FILE" 2>/dev/null; then
			info "sample already present and intact — skipping: mbdump-sample.tar.xz"
		else
			warn "existing sample failed xz -t — fetching a fresh copy"
			rm -f "$SAMPLE_FILE"
			curl -C - -o "$SAMPLE_FILE" -A "$USER_AGENT" -L --retry 3 --retry-delay 5 \
				"$BASE_URL/sample/$SAMPLE_DATE/mbdump-sample.tar.xz" || {
				error "sample download failed (curl exit $?) — partial file left for resume"
				exit 1
			}
		fi
	elif [[ -f "$SAMPLE_FILE" && "$FORCE" == true ]]; then
		rm -f "$SAMPLE_FILE"
	fi
	if [[ ! -f "$SAMPLE_FILE" ]]; then
		curl -C - -o "$SAMPLE_FILE" -A "$USER_AGENT" -L --retry 3 --retry-delay 5 \
			"$BASE_URL/sample/$SAMPLE_DATE/mbdump-sample.tar.xz" || {
			error "sample download failed (curl exit $?) — partial file left for resume"
			exit 1
		}
	fi
	if ! xz -t "$SAMPLE_FILE" 2>/dev/null; then
		rm -f "$SAMPLE_FILE"
		error "sample archive is corrupt (xz -t failed) — removed; re-run to retry"
		exit 1
	fi
	info "sample archive intact (xz -t OK)"
fi

# ---------- Summary -------------------------------------------------------
echo ""
for f in "$DUMP_DIR"/mbdump*.tar.*; do
	[[ -f "$f" ]] || continue
	info "ready: $f ($(du -h "$f" | cut -f1))"
done
echo ""
info "Next step:  mbdb import --dump-dir '$DUMP_DIR'"