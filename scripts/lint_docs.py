#!/usr/bin/env python3
"""Static checks for the hand-authored docs.

Uses only the Python standard library. The checks deliberately target the
static source files; docs.js may enhance pages at runtime, but source links,
anchors, registry entries, and asset versions should already be coherent.
"""

from __future__ import annotations

import gzip
import html
import json
import os
import re
import subprocess
import sys
import tomllib
from html.parser import HTMLParser
from pathlib import Path
from urllib.parse import urlparse


ROOT = Path(__file__).resolve().parents[1]
DOCS = ROOT / "docs"
DOCS_JS = DOCS / "docs.js"
PAGEFIND_FRAGMENTS = DOCS / "pagefind" / "fragment"
RELEASED_VERSION_FILE = DOCS / "released-version.txt"
DOC_VERSION_FILES = [
    ROOT / "README.md",
    DOCS / "index.html",
    DOCS / "quickstart.html",
    DOCS / "tracing.html",
]
SCHEMA_VERSION_CLAIMS = [
    (
        "SQLite durable-core",
        ROOT / "crates" / "lash-sqlite-store" / "src" / "schema.rs",
        "SCHEMA_VERSION",
        [
            (DOCS / "persistence.html", r"SQLite durable-core version (\d+)"),
            (DOCS / "operations.html", r"SQLite durable-core version (\d+)"),
            (
                DOCS / "architecture" / "durable-background-processes.md",
                r"SQLite durable-core schema (\d+)",
            ),
        ],
    ),
    (
        "SQLite process-registry",
        ROOT / "crates" / "lash-sqlite-store" / "src" / "schema.rs",
        "PROCESS_SCHEMA_VERSION",
        [(DOCS / "operations.html", r"process-registry version (\d+)")],
    ),
    (
        "SQLite effect",
        ROOT / "crates" / "lash-sqlite-store" / "src" / "schema.rs",
        "EFFECT_SCHEMA_VERSION",
        [(DOCS / "operations.html", r"effect version (\d+)")],
    ),
    (
        "SQLite trigger",
        ROOT / "crates" / "lash-sqlite-store" / "src" / "schema.rs",
        "TRIGGER_SCHEMA_VERSION",
        [(DOCS / "operations.html", r"trigger version (\d+)")],
    ),
    (
        "Postgres component",
        ROOT / "crates" / "lash-postgres-store" / "src" / "lib.rs",
        "SCHEMA_VERSION",
        [
            (
                DOCS / "persistence.html",
                r"Postgres component version (\d+)",
            ),
            (DOCS / "operations.html", r"Postgres component version (\d+)"),
            (
                DOCS / "architecture" / "durable-background-processes.md",
                r"PostgreSQL schema (\d+) includes",
            ),
        ],
    ),
]
ALPHA_VERSION_RE = re.compile(r"^\d+\.\d+\.\d+-alpha\.\d+$")
EXACT_ALPHA_PIN_RE = re.compile(r'=([0-9]+\.[0-9]+\.[0-9]+-alpha\.[0-9]+)')
ALPHA_TAG_RE = re.compile(r"^v(\d+)\.(\d+)\.(\d+)-alpha\.(\d+)$")

MOVED_STUBS = {
    "architecture.html",
    "architecture/execution-modes.html",
    "plugins-tools.html",
}

ADR_FILENAME_RE = re.compile(r"^(?P<number>\d{4})-[^/]+\.md$")


def check_adr_number_uniqueness(errors: list[str]) -> None:
    """Every ADR filename has one unique four-digit number prefix."""
    by_number: dict[str, list[Path]] = {}
    for path in sorted((DOCS / "adr").glob("*.md")):
        match = ADR_FILENAME_RE.fullmatch(path.name)
        if match is None:
            continue
        by_number.setdefault(match.group("number"), []).append(path)

    for number, paths in sorted(by_number.items()):
        if len(paths) > 1:
            listed = ", ".join(path.relative_to(ROOT).as_posix() for path in paths)
            errors.append(f"docs/adr: duplicate ADR number {number}: {listed}")


def normalize_href(path: str) -> str:
    path = path.strip()
    path = re.sub(r"^[.]/", "", path)
    path = re.sub(r"^/docs/", "", path)
    path = re.sub(r"^docs/", "", path)
    path = path.split("?", 1)[0].split("#", 1)[0]
    path = path.lstrip("/")
    if not path:
        return "index.html"
    if path.endswith("/"):
        path += "index.html"
    if path == "architecture/":
        return "architecture/index.html"
    return path


def registry_block() -> str:
    text = DOCS_JS.read_text(encoding="utf-8")
    start = text.index("const DOCS = [")
    end = text.index("const MOVED_STUB_HREFS", start)
    return text[start:end]


def registry_hrefs() -> list[str]:
    block = registry_block()
    hrefs = [
        normalize_href(m.group(1))
        for m in re.finditer(r'title:\s*"[^"]+"\s*,\s*href:\s*"([^"]+)"', block)
    ]
    return [href for href in hrefs if href != "index.html"]


def registry_titles() -> set[str]:
    block = registry_block()
    titles = {m.group(1).strip().lower() for m in re.finditer(r'title:\s*"([^"]+)"', block)}
    labels = {m.group(1).strip().lower() for m in re.finditer(r'label:\s*"([^"]+)"', block)}
    return {t for t in titles | labels if len(t) > 2}


class PageParser(HTMLParser):
    def __init__(self) -> None:
        super().__init__(convert_charrefs=True)
        self.ids: set[str] = set()
        self.hrefs: list[tuple[str, str]] = []
        self.assets: list[str] = []
        self.pager_hrefs: list[str] = []
        self.strong_texts: list[tuple[str, bool]] = []
        self._stack: list[tuple[str, dict[str, str]]] = []
        self._strong_buf: list[str] | None = None
        self._strong_in_link = False

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        attr = {k: v or "" for k, v in attrs}
        self._stack.append((tag, attr))
        if "id" in attr:
            self.ids.add(attr["id"])
        if "name" in attr:
            self.ids.add(attr["name"])
        if tag == "a" and "href" in attr:
            href = html.unescape(attr["href"])
            self.hrefs.append((href, self._context()))
            if self._in_class("pager__main"):
                self.pager_hrefs.append(href)
        if tag in {"script", "link"}:
            asset = attr.get("src") or attr.get("href")
            if asset:
                self.assets.append(html.unescape(asset))
        if tag == "strong":
            self._strong_buf = []
            self._strong_in_link = self._in_tag("a")

    def handle_endtag(self, tag: str) -> None:
        if tag == "strong" and self._strong_buf is not None:
            text = " ".join("".join(self._strong_buf).split())
            if text:
                self.strong_texts.append((text, self._strong_in_link))
            self._strong_buf = None
        for i in range(len(self._stack) - 1, -1, -1):
            if self._stack[i][0] == tag:
                del self._stack[i:]
                break

    def handle_data(self, data: str) -> None:
        if self._strong_buf is not None:
            self._strong_buf.append(data)

    def _in_tag(self, tag: str) -> bool:
        return any(t == tag for t, _ in self._stack)

    def _in_class(self, cls: str) -> bool:
        for _, attr in self._stack:
            classes = set((attr.get("class") or "").split())
            if cls in classes:
                return True
        return False

    def _context(self) -> str:
        classes: list[str] = []
        for tag, attr in self._stack[-4:]:
            cls = attr.get("class")
            if cls:
                classes.append(f"{tag}.{'.'.join(cls.split())}")
            else:
                classes.append(tag)
        return " > ".join(classes)


def parse_pages() -> dict[Path, PageParser]:
    out: dict[Path, PageParser] = {}
    for path in sorted(DOCS.rglob("*.html")):
        parser = PageParser()
        parser.feed(path.read_text(encoding="utf-8"))
        out[path] = parser
    return out


def is_external_href(href: str) -> bool:
    parsed = urlparse(href)
    return parsed.scheme in {"http", "https", "mailto", "tel"} or href.startswith("//")


def resolve_internal(source: Path, href: str) -> tuple[Path, str]:
    if href.startswith("#"):
        return source, href[1:]
    parsed = urlparse(href)
    raw_path = parsed.path
    fragment = parsed.fragment
    if raw_path.startswith("/docs/"):
        rel = raw_path[len("/docs/") :]
        target = DOCS / normalize_href(rel)
    elif raw_path.startswith("/"):
        target = DOCS / normalize_href(raw_path)
    else:
        target = (source.parent / raw_path).resolve()
        if raw_path == "":
            target = source
        elif raw_path.endswith("/"):
            target = target / "index.html"
    try:
        target = target.resolve().relative_to(DOCS.resolve())
        target = DOCS / target
    except ValueError:
        return target, fragment
    return target, fragment


def check_registry(errors: list[str], pages: dict[Path, PageParser]) -> list[str]:
    hrefs = registry_hrefs()
    seen: set[str] = set()
    canonical: list[str] = []
    for href in hrefs:
        if href in seen:
            errors.append(f"docs.js registry duplicate href: {href}")
            continue
        seen.add(href)
        canonical.append(href)
        if href in MOVED_STUBS:
            errors.append(f"docs.js registry includes moved stub: {href}")
        target = DOCS / href
        if not target.exists():
            errors.append(f"docs.js registry href missing file: {href}")

    expected = {
        path.relative_to(DOCS).as_posix()
        for path, parser in pages.items()
        if "docs-page" in (path.read_text(encoding="utf-8").split("<body", 1)[1].split(">", 1)[0])
    }
    expected.difference_update(MOVED_STUBS)
    missing = sorted(expected - set(canonical))
    extra = sorted(set(canonical) - expected)
    for href in missing:
        errors.append(f"docs-page missing from docs.js registry: {href}")
    for href in extra:
        errors.append(f"docs.js registry href is not a docs-page source: {href}")
    return canonical


def indexed_pages(errors: list[str]) -> set[str] | None:
    """Docs pages present in the shipped Pagefind bundle.

    Pagefind stores one gzip-compressed fragment per indexed page; each holds a
    ``pagefind_dcd``-prefixed JSON blob whose ``url`` is the page path. Reading
    those urls back is the only way to know which pages the checked-in index
    actually covers — ``pagefind-entry.json`` records just a page count, and the
    fragment/index filenames are content hashes. Returns ``None`` (after logging)
    when the bundle is absent so callers skip coverage rather than crash."""
    if not PAGEFIND_FRAGMENTS.is_dir():
        rel = PAGEFIND_FRAGMENTS.relative_to(ROOT).as_posix()
        errors.append(f"pagefind fragment directory missing: {rel}")
        return None
    covered: set[str] = set()
    for fragment in PAGEFIND_FRAGMENTS.glob("*.pf_fragment"):
        raw = gzip.decompress(fragment.read_bytes())
        start = raw.find(b"{")
        if start < 0:
            continue
        url = json.loads(raw[start:]).get("url")
        if url:
            covered.add(normalize_href(url))
    return covered


def check_pagefind_coverage(errors: list[str], canonical: list[str]) -> None:
    """Every nav-registered page must ship in the Pagefind index, so a page can
    never be linked in the docs nav yet be unsearchable. When this fails, the
    index is stale: regenerate it per docs/PUBLISHING.md ("Docs search index")."""
    covered = indexed_pages(errors)
    if covered is None:
        return
    for href in canonical:
        if href not in covered:
            errors.append(
                f"docs.js registry page missing from pagefind index "
                f"(regenerate docs/pagefind — see docs/PUBLISHING.md): {href}"
            )


def check_links(errors: list[str], pages: dict[Path, PageParser]) -> None:
    ids_by_path = {path: parser.ids for path, parser in pages.items()}
    for source, parser in pages.items():
        for href, context in parser.hrefs:
            if not href or is_external_href(href):
                continue
            target, fragment = resolve_internal(source, href)
            rel_source = source.relative_to(ROOT).as_posix()
            try:
                rel_target = target.relative_to(DOCS).as_posix()
            except ValueError:
                continue
            if rel_target in MOVED_STUBS:
                errors.append(f"{rel_source}: link points at moved stub {href!r}")
            if not target.exists():
                errors.append(f"{rel_source}: missing internal href {href!r} ({context})")
                continue
            if fragment and target.suffix == ".html":
                if fragment not in ids_by_path.get(target, set()):
                    errors.append(f"{rel_source}: missing anchor {href!r}")


def check_asset_versions(errors: list[str], pages: dict[Path, PageParser]) -> None:
    versions: dict[str, set[str]] = {}
    for parser in pages.values():
        for asset in parser.assets:
            parsed = urlparse(asset)
            if parsed.scheme or not parsed.query:
                continue
            qs = dict(part.split("=", 1) for part in parsed.query.split("&") if "=" in part)
            if "v" not in qs:
                continue
            name = os.path.basename(parsed.path)
            versions.setdefault(name, set()).add(qs["v"])

    js = DOCS_JS.read_text(encoding="utf-8")
    for const, asset in {
        "SCENE_ASSET_VERSION": "scene.js",
        "MERMAID_LOADER_VERSION": "mermaid.js",
    }.items():
        m = re.search(rf'const\s+{const}\s*=\s*"([^"]+)"', js)
        if m:
            versions.setdefault(asset, set()).add(m.group(1))

    for asset, seen in sorted(versions.items()):
        if len(seen) > 1:
            errors.append(f"asset {asset} has inconsistent ?v= values: {', '.join(sorted(seen))}")


def check_pagers(errors: list[str], canonical: list[str], pages: dict[Path, PageParser]) -> None:
    index = {href: i for i, href in enumerate(canonical)}
    for path, parser in pages.items():
        href = path.relative_to(DOCS).as_posix()
        if href not in index:
            continue
        if not parser.pager_hrefs:
            continue
        expected: list[str] = []
        i = index[href]
        if i > 0:
            expected.append(canonical[i - 1])
        if i + 1 < len(canonical):
            expected.append(canonical[i + 1])
        actual = [normalize_href(resolve_internal(path, h)[0].relative_to(DOCS).as_posix()) for h in parser.pager_hrefs]
        if actual != expected:
            errors.append(f"{path.relative_to(ROOT).as_posix()}: static pager conflicts with docs.js order")


def check_bold_cross_refs(errors: list[str], pages: dict[Path, PageParser]) -> None:
    titles = registry_titles()
    ignored = {"note", "contract", "gotcha", "use when", "avoid"}
    for path, parser in pages.items():
        for text, in_link in parser.strong_texts:
            if in_link:
                continue
            normalized = text.strip().lower().rstrip(".:")
            if normalized in ignored:
                continue
            if normalized in titles:
                errors.append(
                    f"{path.relative_to(ROOT).as_posix()}: strong text looks like an unlinked docs reference: {text!r}"
                )


def check_release_version_pins(errors: list[str]) -> None:
    """Gate install snippets against the latest known release.

    A checked-in version is the offline authority. In a full clone its matching
    tag must exist and no newer release tag may be present. When that tag is not
    available (for example in an offline shallow checkout), the checked-in value
    remains authoritative rather than making docs lint depend on the network.
    """
    try:
        expected = RELEASED_VERSION_FILE.read_text(encoding="utf-8").strip()
    except OSError as error:
        errors.append(f"{RELEASED_VERSION_FILE.relative_to(ROOT)}: cannot read: {error}")
        return
    if not ALPHA_VERSION_RE.fullmatch(expected):
        errors.append(
            f"{RELEASED_VERSION_FILE.relative_to(ROOT)}: expected an alpha release version, got {expected!r}"
        )
        return

    for path in DOC_VERSION_FILES:
        pins = EXACT_ALPHA_PIN_RE.findall(path.read_text(encoding="utf-8"))
        rel = path.relative_to(ROOT).as_posix()
        if not pins:
            errors.append(f"{rel}: no exact Lash alpha release pin found")
            continue
        for pin in sorted(set(pins)):
            if pin != expected:
                errors.append(
                    f"{rel}: release pin {pin} does not match {RELEASED_VERSION_FILE.relative_to(ROOT)} ({expected})"
                )

    try:
        result = subprocess.run(
            ["git", "tag", "--list", "v*"],
            cwd=ROOT,
            check=False,
            capture_output=True,
            text=True,
        )
    except OSError:
        return
    if result.returncode != 0:
        return

    tagged: list[tuple[tuple[int, int, int, int], str]] = []
    for tag in result.stdout.splitlines():
        match = ALPHA_TAG_RE.fullmatch(tag.strip())
        if match:
            tagged.append((tuple(int(part) for part in match.groups()), tag[1:]))
    if not tagged:
        return
    latest_key, latest = max(tagged)
    expected_match = ALPHA_TAG_RE.fullmatch(f"v{expected}")
    assert expected_match is not None
    expected_key = tuple(int(part) for part in expected_match.groups())
    if latest_key > expected_key or (
        expected in {version for _, version in tagged} and latest != expected
    ):
        errors.append(
            f"{RELEASED_VERSION_FILE.relative_to(ROOT)}: {expected} is not the latest release tag ({latest})"
        )


FORMAT_VERSION_SPAN = re.compile(
    r"<span\s+data-format-version=\"[A-Za-z0-9_]+\"\s*>(.*?)</span>", re.DOTALL
)


def unwrap_format_version_spans(text: str) -> str:
    """Read a page as if its version claims were bare numbers.

    ``scripts/check_format_versions.py`` requires every current version claim on
    a live page to be wrapped in ``<span data-format-version="...">`` so that it
    can be compared against the source tree. The claim patterns below predate
    that wrapper and describe the prose around the number, so they are run over
    an unwrapped copy rather than each being taught the markup.
    """
    return FORMAT_VERSION_SPAN.sub(lambda match: match.group(1), text)


def check_schema_version_claims(errors: list[str]) -> None:
    """Keep current-schema docs pinned to the Rust constants hosts enforce."""
    for label, source, constant, claims in SCHEMA_VERSION_CLAIMS:
        source_text = source.read_text(encoding="utf-8")
        constant_matches = re.findall(
            rf"(?m)^(?:pub(?:\(crate\))?\s+)?const\s+{re.escape(constant)}:\s*i32\s*=\s*(\d+);$",
            source_text,
        )
        source_rel = source.relative_to(ROOT).as_posix()
        if len(constant_matches) != 1:
            errors.append(
                f"{source_rel}: expected exactly one i32 {constant}, found {len(constant_matches)}"
            )
            continue
        expected = int(constant_matches[0])

        for doc, pattern in claims:
            doc_text = unwrap_format_version_spans(doc.read_text(encoding="utf-8"))
            stated = re.findall(pattern, doc_text)
            doc_rel = doc.relative_to(ROOT).as_posix()
            if len(stated) != 1:
                errors.append(
                    f"{doc_rel}: expected exactly one current {label} schema version claim, found {len(stated)}"
                )
                continue
            actual = int(stated[0])
            if actual != expected:
                errors.append(
                    f"{doc_rel}: {label} schema version {actual} does not match "
                    f"{source_rel} {constant} ({expected})"
                )


def check_remote_protocol_version(errors: list[str]) -> None:
    """Keep the canonical remote-protocol page aligned with exact negotiation."""
    source = ROOT / "crates" / "lash-remote-protocol" / "src" / "lib.rs"
    doc = DOCS / "remote-protocol.html"
    source_matches = re.findall(
        r"(?m)^pub const REMOTE_PROTOCOL_VERSION:\s*u32\s*=\s*(\d+);$",
        source.read_text(encoding="utf-8"),
    )
    if len(source_matches) != 1:
        errors.append(
            "crates/lash-remote-protocol/src/lib.rs: expected exactly one u32 "
            f"REMOTE_PROTOCOL_VERSION, found {len(source_matches)}"
        )
        return
    documented = re.findall(
        r"REMOTE_PROTOCOL_VERSION\s*==\s*(\d+)",
        unwrap_format_version_spans(doc.read_text(encoding="utf-8")),
    )
    if len(documented) != 1:
        errors.append(
            "docs/remote-protocol.html: expected exactly one documented "
            f"REMOTE_PROTOCOL_VERSION literal, found {len(documented)}"
        )
        return
    expected = int(source_matches[0])
    actual = int(documented[0])
    if actual != expected:
        errors.append(
            "docs/remote-protocol.html: remote protocol version "
            f"{actual} does not match crates/lash-remote-protocol/src/lib.rs "
            f"REMOTE_PROTOCOL_VERSION ({expected})"
        )


def check_quickstart_contract(errors: list[str]) -> None:
    """Keep the quickstart aligned with the compiled in-repo facade."""
    quickstart = (DOCS / "quickstart.html").read_text(encoding="utf-8")
    required_claims = [
        "minimal embedded-agent program",
        "app-facing <code>lash-runtime</code> facade",
        "Rust library import is still <code>use lash::...</code>",
        "quickstart intentionally leaves most host features out",
    ]
    for claim in required_claims:
        if claim not in quickstart:
            errors.append(f"docs/quickstart.html: missing scope or crate-name claim {claim!r}")
    # The quickstart teaches embedding, so it must never route the reader
    # through installing or invoking a CLI host binary.
    forbidden_cli_markers = [
        "install_lash",
        "cargo install",
        "curl -fssl",
        "~/.lash",
        "$ lash ",
    ]
    lowered = quickstart.lower()
    for marker in forbidden_cli_markers:
        if marker in lowered:
            errors.append(
                "docs/quickstart.html: quickstart must stay embedded and free of CLI-host "
                f"install or invocation steps (found {marker!r})"
            )

    facade = tomllib.loads((ROOT / "crates" / "lash" / "Cargo.toml").read_text())
    if facade.get("package", {}).get("name") != "lash-runtime":
        errors.append("crates/lash/Cargo.toml: facade package must be named lash-runtime")
    if facade.get("lib", {}).get("name") != "lash":
        errors.append("crates/lash/Cargo.toml: lash-runtime library must be imported as lash")

    snippets = tomllib.loads(
        (ROOT / "examples" / "docs-snippets" / "Cargo.toml").read_text()
    )
    if "lash" not in snippets.get("dependencies", {}):
        errors.append("examples/docs-snippets/Cargo.toml: compiled examples must depend on lash")
    source = (SNIPPETS_SRC / "quickstart.rs").read_text(encoding="utf-8")
    if "use lash::{" not in source:
        errors.append("examples/docs-snippets/src/quickstart.rs: expected a lash library import")


SNIPPETS_SRC = ROOT / "examples" / "docs-snippets" / "src"
REGION_START = re.compile(r"^(\s*)// docs:start:([\w-]+)\s*$")
PRE_BLOCK = re.compile(r"<pre([^>]*)><code>(.*?)</code></pre>", re.S)


def snippet_regions(errors: list[str]) -> dict[str, dict[str, str]]:
    """Extract `// docs:start:<id>` .. `// docs:end:<id>` regions, dedented by
    the indentation of their start marker, from the docs-snippets crate."""
    regions: dict[str, dict[str, str]] = {}
    for path in sorted(SNIPPETS_SRC.glob("*.rs")):
        module = path.stem
        rel = path.relative_to(ROOT).as_posix()
        current: tuple[str, str, list[str]] | None = None
        for line in path.read_text(encoding="utf-8").splitlines():
            started = REGION_START.match(line)
            if current is None:
                if started:
                    current = (started.group(2), started.group(1), [])
                continue
            rid, indent, body = current
            if re.match(rf"^\s*// docs:end:{re.escape(rid)}\s*$", line):
                text = "\n".join(
                    l[len(indent):] if l.startswith(indent) else l for l in body
                )
                if rid in regions.get(module, {}):
                    errors.append(f"{rel}: duplicate snippet region {rid!r}")
                regions.setdefault(module, {})[rid] = text.strip("\n")
                current = None
            elif started:
                errors.append(f"{rel}: region {rid!r} not closed before {started.group(2)!r}")
            else:
                body.append(line)
        if current is not None:
            errors.append(f"{rel}: unterminated snippet region {current[0]!r}")
    return regions


def check_code_snippets(errors: list[str], fix: bool) -> None:
    """Every `<pre>` on a published top-level page declares what it is:
    `data-snippet` blocks mirror a compiled region of the docs-snippets crate
    verbatim (`cargo check -p docs-snippets` keeps them building); `data-lang`
    blocks are display-only (shell transcripts, Lashlang, API-shape excerpts).

    `data-snippet` blocks are compiled-and-synced wherever they appear, including
    the `architecture/` sub-pages; the "declare yourself" rule stays scoped to
    top-level pages so hand-authored architecture prose blocks are left alone."""
    regions = snippet_regions(errors)
    referenced: set[str] = set()
    for path in sorted(DOCS.rglob("*.html")):
        rel = path.relative_to(ROOT).as_posix()
        enforce_declaration = path.parent == DOCS
        text = path.read_text(encoding="utf-8")
        changed = False

        def replace(m: re.Match[str], enforce_declaration: bool = enforce_declaration) -> str:
            nonlocal changed
            attrs, body = m.group(1), m.group(2)
            snippet = re.search(r'data-snippet="([^"]+)"', attrs)
            if snippet is None:
                if enforce_declaration and not re.search(r'data-lang="[^"]+"', attrs):
                    errors.append(
                        f"{rel}: <pre> needs data-snippet (compiled, synced) or data-lang (display-only)"
                    )
                return m.group(0)
            ref = snippet.group(1)
            module, _, rid = ref.partition("#")
            source = regions.get(module, {}).get(rid)
            if source is None:
                errors.append(
                    f"{rel}: data-snippet {ref!r} has no region in examples/docs-snippets/src/{module}.rs"
                )
                return m.group(0)
            referenced.add(ref)
            if html.unescape(body).strip("\n") != source:
                if fix:
                    changed = True
                    return f"<pre{attrs}><code>{html.escape(source, quote=False)}</code></pre>"
                errors.append(
                    f"{rel}: <pre data-snippet={ref!r}> drifted from examples/docs-snippets/src/{module}.rs"
                    " (run scripts/lint_docs.py --fix-snippets)"
                )
            return m.group(0)

        new_text = PRE_BLOCK.sub(replace, text)
        if fix and changed:
            path.write_text(new_text, encoding="utf-8")

    check_readme_snippets(errors, regions, referenced, fix)

    for module, by_id in sorted(regions.items()):
        for rid in sorted(by_id):
            if f"{module}#{rid}" not in referenced:
                errors.append(
                    f"examples/docs-snippets/src/{module}.rs: region {rid!r} is not embedded by any docs page"
                )


# README ```rust fences, in document order, mirror these compiled regions.
README_SNIPPETS = ["index#hello-lash"]


def check_readme_snippets(
    errors: list[str],
    regions: dict[str, dict[str, str]],
    referenced: set[str],
    fix: bool,
) -> None:
    readme = ROOT / "README.md"
    text = readme.read_text(encoding="utf-8")
    fences = list(re.finditer(r"```rust\n(.*?)```\n", text, re.S))
    if len(fences) != len(README_SNIPPETS):
        errors.append(
            f"README.md: expected {len(README_SNIPPETS)} ```rust fences (see README_SNIPPETS in scripts/lint_docs.py), found {len(fences)}"
        )
        return
    changed = False
    for fence, ref in zip(reversed(fences), reversed(README_SNIPPETS)):
        module, _, rid = ref.partition("#")
        source = regions.get(module, {}).get(rid)
        if source is None:
            errors.append(f"README.md: snippet ref {ref!r} has no region in examples/docs-snippets/src/{module}.rs")
            continue
        referenced.add(ref)
        if fence.group(1).strip("\n") != source:
            if fix:
                text = text[: fence.start(1)] + source + "\n" + text[fence.end(1) :]
                changed = True
            else:
                errors.append(
                    f"README.md: ```rust fence drifted from examples/docs-snippets/src/{module}.rs region {rid!r}"
                    " (run scripts/lint_docs.py --fix-snippets)"
                )
    if changed:
        readme.write_text(text, encoding="utf-8")


def main() -> int:
    fix = "--fix-snippets" in sys.argv[1:]
    errors: list[str] = []
    check_adr_number_uniqueness(errors)
    pages = parse_pages()
    canonical = check_registry(errors, pages)
    check_pagefind_coverage(errors, canonical)
    check_links(errors, pages)
    check_asset_versions(errors, pages)
    check_pagers(errors, canonical, pages)
    check_bold_cross_refs(errors, pages)
    check_release_version_pins(errors)
    check_schema_version_claims(errors)
    check_remote_protocol_version(errors)
    check_quickstart_contract(errors)
    check_code_snippets(errors, fix)
    if errors:
        for err in errors:
            print(f"docs lint: {err}", file=sys.stderr)
        return 1
    print(f"docs lint: ok ({len(pages)} html pages, {len(canonical)} registry pages)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
