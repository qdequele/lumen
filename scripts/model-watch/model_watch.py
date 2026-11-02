#!/usr/bin/env python3
"""Model release watcher: detect new models and providers in public catalogs.

Fetches models.dev, LiteLLM's price JSON and OpenRouter's model lists, diffs
them against the committed snapshot (snapshot.json next to this file), and
classifies every addition against what LUMEN supports, read from
crates/providers/src/kind.rs and docs/providers.md:

  (a) new model on a provider LUMEN already supports: config only.
  (b) new provider with an OpenAI-compatible base URL: candidate for a new
      ProviderKind variant in the OpenAI-compatible family.
  (c) new provider with a native API, or a new rerank model (or an embedding
      model behind a non OpenAI-compatible API) from an unsupported provider:
      needs a real connector.

It then opens or updates one GitHub issue per provider of type (b)/(c) and one
rolling issue for all type (a) additions, and rewrites the snapshot.

Python standard library only. See docs/operations/model-watch.md.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import re
import sys
import time
import urllib.error
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path

HERE = Path(__file__).resolve().parent
DEFAULT_REPO_ROOT = HERE.parent.parent
SNAPSHOT_VERSION = 1

# U+2014 is banned repo-wide (CI no-em-dashes job). Catalog text flows into the
# snapshot and into issue bodies, so it is replaced on the way in.
EM_DASH = "\u2014"
CHECK = "✅"

SOURCES = {
    "models.dev": "https://models.dev/api.json",
    "litellm": "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json",
    "openrouter": "https://openrouter.ai/api/v1/models",
    "openrouter_embeddings": "https://openrouter.ai/api/v1/embeddings/models",
}
# Not a snapshot source: only a hint for which LiteLLM providers speak the
# OpenAI wire format, and their base URL. Best effort.
LITELLM_OPENAI_LIKE = "https://raw.githubusercontent.com/BerriAI/litellm/main/litellm/llms/openai_like/providers.json"

# Minimum plausible size per source. A response below this is treated as a
# shape change (or an outage page), never as a mass removal.
MIN_MODELS = {"models.dev": 1000, "litellm": 500, "openrouter": 100, "openrouter_embeddings": 5}
# A source shrinking below this fraction of its previous size is refused for
# the same reason: recording it would re-announce everything once it recovers.
MIN_RATIO = 0.5

CAPS = ("chat", "embedding", "rerank")
SCOPE_LABEL = {"chat": "scope: chat", "embedding": "scope: embedding", "rerank": "scope: reranking"}
ROLLING_MARKER = "[model-watch:supported-models]"
MAX_LISTED = 60
# GitHub rejects an issue or comment body over 65,536 characters with a 422,
# which would fail the run before the snapshot is written, every day. Many
# kinds with large additions at once (an id rename upstream) can get there
# despite the per-table MAX_LISTED, so every body is capped as a whole too.
MAX_BODY = 60_000

USER_AGENT = "lumen-model-watch (+https://github.com/qdequele/lumen)"


class ShapeError(Exception):
    """A catalog no longer has the JSON shape this script expects."""


def clean(text: str) -> str:
    return text.replace(EM_DASH, "-")


def norm(provider_id: str) -> str:
    """Normalize a provider id so 'together_ai', 'together-ai', 'TogetherAI' match."""
    return re.sub(r"[^a-z0-9]", "", provider_id.lower())


def norm_url(url: str | None) -> str | None:
    if not url:
        return None
    return url.strip().lower().rstrip("/")


# --- LUMEN capability model --------------------------------------------------


@dataclass
class Lumen:
    """What LUMEN supports, parsed from the source tree."""

    kinds: list[str]
    openai_compatible: set[str]
    default_base_urls: dict[str, str]
    no_embeddings: set[str]
    caps: dict[str, set[str]]
    aliases: dict[str, str]
    prefix_aliases: dict[str, str]
    warnings: list[str] = field(default_factory=list)

    def resolve(self, provider_id: str, base_url: str | None = None) -> str | None:
        """Map a catalog provider id (and optional base URL) to a LUMEN kind."""
        key = norm(provider_id)
        by_norm = {norm(k): k for k in self.kinds}
        if key in by_norm:
            return by_norm[key]
        if key in self.aliases:
            return self.aliases[key]
        lowered = provider_id.lower()
        for prefix, kind in self.prefix_aliases.items():
            if lowered.startswith(prefix):
                return kind
        url = norm_url(base_url)
        if url:
            for kind, default in self.default_base_urls.items():
                if norm_url(default) == url:
                    return kind
        return None


def _match_body(source: str, fn_name: str) -> str:
    m = re.search(rf"fn {fn_name}\(self\)[^{{]*\{{(.*?)\n    \}}", source, re.S)
    if not m:
        raise ShapeError(f"kind.rs: could not find fn {fn_name}")
    return m.group(1)


def parse_kind_rs(text: str) -> tuple[list[str], set[str], dict[str, str], set[str]]:
    """Return (kinds, openai_compatible, default_base_urls, no_embeddings) from kind.rs."""
    pairs = re.findall(r'ProviderKind::(\w+) => "(\w+)"', _match_body(text, "as_str"))
    if not pairs:
        raise ShapeError("kind.rs: as_str has no arms")
    variant_to_name = dict(pairs)

    def names(fn_name: str) -> set[str]:
        body = _match_body(text, fn_name)
        return {variant_to_name[v] for v in re.findall(r"ProviderKind::(\w+)", body) if v in variant_to_name}

    urls = {
        variant_to_name[v]: url
        for v, url in re.findall(r'ProviderKind::(\w+) => Some\("([^"]+)"\)', _match_body(text, "default_base_url"))
    }
    return [n for _, n in pairs], names("is_openai_compatible"), urls, names("supports_embeddings")


def parse_docs_tables(text: str) -> dict[str, set[str]]:
    """Read every '| `kind` | Chat | Embed | Rerank ...' table in docs/providers.md."""
    column_cap = {"chat": "chat", "embed": "embedding", "rerank": "rerank"}
    caps: dict[str, set[str]] = {}
    header: list[str] | None = None
    for line in text.splitlines():
        if not line.startswith("|"):
            header = None
            continue
        cells = [c.strip() for c in line.strip().strip("|").split("|")]
        if cells and cells[0] == "`kind`":
            header = [c.lower() for c in cells]
            continue
        if header is None or set(cells[0]) <= {"-", ":", " "}:
            continue
        kind_match = re.fullmatch(r"`(\w+)`", cells[0])
        if not kind_match:
            continue
        kind = kind_match.group(1)
        found = caps.setdefault(kind, set())
        for col, cell in zip(header[1:], cells[1:]):
            if col in column_cap and CHECK in cell:
                found.add(column_cap[col])
    return caps


def load_lumen(repo_root: Path) -> Lumen:
    kind_rs = (repo_root / "crates/providers/src/kind.rs").read_text(encoding="utf-8")
    docs = (repo_root / "docs/providers.md").read_text(encoding="utf-8")
    config = json.loads((HERE / "providers.json").read_text(encoding="utf-8"))
    kinds, compat, urls, embedless = parse_kind_rs(kind_rs)
    doc_caps = parse_docs_tables(docs)
    warnings = []
    caps: dict[str, set[str]] = {}
    for kind in kinds:
        if kind in doc_caps:
            found = set(doc_caps[kind])
        else:
            warnings.append(f"kind `{kind}` is missing from the docs/providers.md capability tables")
            found = {"chat", "embedding"} if kind in compat else set()
        if kind in embedless:
            found.discard("embedding")
        found |= set(config.get("extra_capabilities", {}).get(kind, []))
        caps[kind] = found
    return Lumen(
        kinds=kinds,
        openai_compatible=compat,
        default_base_urls=urls,
        no_embeddings=embedless,
        caps=caps,
        aliases={norm(k): v for k, v in config.get("aliases", {}).items()},
        prefix_aliases={k.lower(): v for k, v in config.get("prefix_aliases", {}).items()},
        warnings=warnings,
    )


# --- Catalog parsing ---------------------------------------------------------


@dataclass
class ProviderMeta:
    name: str
    api: str | None = None
    npm: str | None = None
    doc: str | None = None
    openai_compatible: bool | None = None


Catalog = dict[str, dict[str, str]]  # provider id -> {model id: capability}


def _require(cond: bool, message: str) -> None:
    if not cond:
        raise ShapeError(message)


def parse_models_dev(data: object) -> tuple[Catalog, dict[str, ProviderMeta]]:
    _require(isinstance(data, dict), "models.dev: top level is not an object")
    type_cap = {"chat": "chat", "embedding": "embedding", "reranking": "rerank"}
    catalog: Catalog = {}
    meta: dict[str, ProviderMeta] = {}
    for pid, provider in data.items():
        _require(isinstance(provider, dict) and isinstance(provider.get("models"), dict),
                 f"models.dev: provider {pid!r} has no 'models' object")
        pid = clean(pid)
        npm = provider.get("npm")
        api = provider.get("api")
        meta[pid] = ProviderMeta(
            name=clean(str(provider.get("name") or pid)),
            api=api,
            npm=npm,
            doc=provider.get("doc"),
            # The AI SDK's generic OpenAI-compatible package, with a base URL,
            # is the catalog's own statement that the wire format is OpenAI's.
            openai_compatible=bool(api) and npm == "@ai-sdk/openai-compatible",
        )
        models: dict[str, str] = {}
        for mid, model in provider["models"].items():
            if not isinstance(model, dict):
                continue
            kind = model.get("type")
            if kind is None:
                outputs = (model.get("modalities") or {}).get("output") or []
                cap = "chat" if "text" in outputs else None
            else:
                cap = type_cap.get(kind)
            if cap:
                models[clean(mid)] = cap
        catalog[pid] = models
    return catalog, meta


def parse_litellm(data: object) -> Catalog:
    _require(isinstance(data, dict), "litellm: top level is not an object")
    mode_cap = {"chat": "chat", "completion": "chat", "responses": "chat", "embedding": "embedding", "rerank": "rerank"}
    catalog: Catalog = {}
    with_provider = 0
    for mid, spec in data.items():
        if mid == "sample_spec" or not isinstance(spec, dict):
            continue
        provider = spec.get("litellm_provider")
        if not isinstance(provider, str):
            continue
        with_provider += 1
        cap = mode_cap.get(spec.get("mode") or "")
        if cap:
            catalog.setdefault(clean(provider), {})[clean(mid)] = cap
    _require(with_provider > 0, "litellm: no entry carries 'litellm_provider'")
    return catalog


def parse_openrouter(data: object, cap: str) -> Catalog:
    _require(isinstance(data, dict) and isinstance(data.get("data"), list), "openrouter: no 'data' list")
    models: dict[str, str] = {}
    for item in data["data"]:
        _require(isinstance(item, dict) and isinstance(item.get("id"), str), "openrouter: entry without 'id'")
        if cap == "chat":
            outputs = (item.get("architecture") or {}).get("output_modalities") or ["text"]
            if "text" not in outputs:
                continue
        models[clean(item["id"])] = cap
    # Every OpenRouter model is served by one API: the `openrouter` kind.
    return {"openrouter": models}


def count(catalog: Catalog) -> int:
    return sum(len(m) for m in catalog.values())


# --- Fetching ----------------------------------------------------------------


def fetch_json(url: str, attempts: int = 3) -> object:
    last: Exception | None = None
    for attempt in range(attempts):
        try:
            req = urllib.request.Request(url, headers={"User-Agent": USER_AGENT, "Accept": "application/json"})
            with urllib.request.urlopen(req, timeout=60) as resp:
                return json.loads(resp.read())
        except (urllib.error.URLError, TimeoutError, json.JSONDecodeError) as err:
            last = err
            time.sleep(2 * (attempt + 1))
    raise ShapeError(f"could not fetch {url}: {last}")


def load_raw(name: str, url: str, source_dir: Path | None) -> object:
    if source_dir is not None:
        return json.loads((source_dir / f"{name}.json").read_text(encoding="utf-8"))
    return fetch_json(url)


@dataclass
class Fetched:
    catalogs: dict[str, Catalog]
    meta: dict[str, dict[str, ProviderMeta]]  # source -> provider -> meta
    errors: dict[str, str]


def fetch_all(source_dir: Path | None) -> Fetched:
    catalogs: dict[str, Catalog] = {}
    meta: dict[str, dict[str, ProviderMeta]] = {}
    errors: dict[str, str] = {}
    for name, url in SOURCES.items():
        try:
            raw = load_raw(name, url, source_dir)
            if name == "models.dev":
                catalogs[name], meta[name] = parse_models_dev(raw)
            elif name == "litellm":
                catalogs[name] = parse_litellm(raw)
            else:
                catalogs[name] = parse_openrouter(raw, "chat" if name == "openrouter" else "embedding")
            got = count(catalogs[name])
            if got < MIN_MODELS[name]:
                del catalogs[name]
                raise ShapeError(f"{name}: only {got} models (expected at least {MIN_MODELS[name]})")
        except Exception as err:  # noqa: BLE001 - any parse failure means a shape change
            errors[name] = f"{name}: {type(err).__name__}: {err}"
    # LiteLLM provider metadata from its OpenAI-like registry (optional).
    lite_meta: dict[str, ProviderMeta] = {}
    try:
        registry = load_raw("litellm_openai_like", LITELLM_OPENAI_LIKE, source_dir)
        if isinstance(registry, dict):
            for pid, spec in registry.items():
                if isinstance(spec, dict) and spec.get("base_url"):
                    lite_meta[norm(pid)] = ProviderMeta(name=clean(pid), api=spec["base_url"], openai_compatible=True)
    except (ShapeError, OSError, ValueError) as err:
        print(f"warning: LiteLLM openai_like registry unavailable ({err}); compatibility hints reduced", file=sys.stderr)
    if "litellm" in catalogs:
        meta["litellm"] = {
            pid: lite_meta.get(norm(pid), ProviderMeta(name=pid)) for pid in catalogs["litellm"]
        }
    return Fetched(catalogs, meta, errors)


# --- Diff and classification -------------------------------------------------


@dataclass
class Addition:
    model: str
    cap: str
    sources: set[str]


@dataclass
class SupportedChange:
    """A type (a) addition, grouped by LUMEN kind."""

    kind: str
    model: str
    cap: str
    sources: set[str]
    gap: bool  # the kind does not serve this capability yet
    upstream_compatible: bool = False


@dataclass
class ProviderFinding:
    """A type (b) or (c) provider."""

    key: str  # normalized provider id, the dedupe key
    name: str
    ids: dict[str, str]  # source -> raw provider id
    api: str | None
    npm: str | None
    doc: str | None
    openai_compatible: bool | None
    is_new: bool
    # Keyed by (model id, capability): catalogs can disagree on one id's
    # capability, and each one counts for category, labels and priority.
    models: dict[tuple[str, str], Addition]

    @property
    def caps(self) -> set[str]:
        return {a.cap for a in self.models.values()}

    @property
    def category(self) -> str:
        if "rerank" in self.caps or not self.openai_compatible:
            return "c"
        return "b"

    @property
    def marker(self) -> str:
        return f"[model-watch:provider:{self.key}]"


@dataclass
class Report:
    supported: list[SupportedChange]
    providers: list[ProviderFinding]
    removed: dict[str, int]
    source_errors: dict[str, str]


def diff_and_classify(old: dict[str, Catalog], fetched: Fetched, lumen: Lumen) -> Report:
    known_keys = {norm(pid) for cat in old.values() for pid in cat}
    # Keyed by (kind, model, capability): when catalogs report one model id
    # with different capabilities, each keeps its own row and gap status.
    supported: dict[tuple[str, str, str], SupportedChange] = {}
    findings: dict[str, ProviderFinding] = {}
    removed: dict[str, int] = {}

    for source, catalog in fetched.catalogs.items():
        before = old.get(source, {})
        removed[source] = sum(
            1 for pid, models in before.items() for mid in models if mid not in catalog.get(pid, {})
        )
        source_meta = fetched.meta.get(source, {})
        for pid, models in catalog.items():
            prev = before.get(pid)
            added = {mid: cap for mid, cap in models.items() if prev is None or mid not in prev}
            if not added:
                continue
            meta = source_meta.get(pid, ProviderMeta(name=pid))
            kind = lumen.resolve(pid, meta.api)
            if kind is not None:
                for mid, cap in added.items():
                    entry = supported.setdefault(
                        (kind, mid, cap),
                        SupportedChange(kind, mid, cap, set(), cap not in lumen.caps.get(kind, set())),
                    )
                    entry.sources.add(source)
                    entry.upstream_compatible = entry.upstream_compatible or bool(meta.openai_compatible)
                continue
            key = norm(pid)
            is_new = key not in known_keys
            if not is_new:
                # Existing unsupported providers are baseline noise for chat;
                # only first-class embedding/rerank additions are surfaced.
                added = {mid: cap for mid, cap in added.items() if cap != "chat"}
                if not added:
                    continue
            finding = findings.get(key)
            if finding is None:
                finding = findings[key] = ProviderFinding(
                    key=key, name=meta.name, ids={}, api=meta.api, npm=meta.npm, doc=meta.doc,
                    openai_compatible=meta.openai_compatible, is_new=is_new, models={},
                )
            finding.ids[source] = pid
            finding.api = finding.api or meta.api
            finding.npm = finding.npm or meta.npm
            finding.doc = finding.doc or meta.doc
            if meta.openai_compatible:
                finding.openai_compatible = True
            if source == "models.dev":
                finding.name = meta.name  # the friendliest display name
            for mid, cap in added.items():
                finding.models.setdefault((mid, cap), Addition(mid, cap, set())).sources.add(source)

    return Report(
        supported=sorted(supported.values(), key=lambda c: (c.kind, c.cap, c.model)),
        providers=sorted(findings.values(), key=lambda f: (f.category, f.key)),
        removed=removed,
        source_errors=dict(fetched.errors),
    )


# --- Issue rendering ---------------------------------------------------------

FOOTER = (
    "\n---\n_Filed by the model release watcher (`.github/workflows/model-watch.yml`, "
    "see `docs/operations/model-watch.md`). Close this issue to silence it: the watcher "
    "never reopens a closed provider issue._\n"
)

CATEGORY_TEXT = {
    "b": "**(b) OpenAI-compatible provider**: candidate for a new `ProviderKind` variant in the "
    "OpenAI-compatible family (no new translation code).",
    "c": "**(c) Needs a real connector**: native API, a rerank model, or an API whose wire format "
    "the catalogs do not identify as OpenAI-compatible.",
}

NEXT_STEPS = {
    "b": (
        "- [ ] Confirm the base URL and that `/chat/completions` (and `/embeddings`, if listed) speak the OpenAI format.\n"
        "- [ ] Add the variant to `ProviderKind` in `crates/providers/src/kind.rs`: `as_str`, "
        "`is_openai_compatible`, `default_base_url`, and `supports_embeddings` if it has no embeddings API.\n"
        "- [ ] Add a row to the OpenAI-compatible table in `docs/providers.md` and a CHANGELOG entry.\n"
        "- [ ] Until then it already works with `kind = \"openai\"` plus a `base_url` override.\n"
    ),
    "c": (
        "- [ ] Read the provider's API docs and decide whether LUMEN should support it (write an ADR if it needs a new pattern).\n"
        "- [ ] Implement the connector in `crates/providers` (the `provider-integrator` agent covers the repeatable part), "
        "with wiremock tests and cancellation coverage.\n"
        "- [ ] Document it in `docs/providers.md` and add a CHANGELOG entry.\n"
    ),
}


def _models_table(rows: list[tuple[str, str, str]]) -> str:
    lines = ["| Model | Capability | Seen in |", "|---|---|---|"]
    for model, cap, seen in rows[:MAX_LISTED]:
        lines.append(f"| `{model}` | {cap} | {seen} |")
    if len(rows) > MAX_LISTED:
        lines.append(f"| ... and {len(rows) - MAX_LISTED} more | | |")
    return "\n".join(lines) + "\n"


def _finding_rows(finding: ProviderFinding) -> list[tuple[str, str, str]]:
    order = {cap: i for i, cap in enumerate(("rerank", "embedding", "chat"))}
    adds = sorted(finding.models.values(), key=lambda a: (order[a.cap], a.model))
    return [(a.model, a.cap, ", ".join(sorted(a.sources))) for a in adds]


def provider_labels(finding: ProviderFinding) -> list[str]:
    priority = "priority: medium" if finding.caps & {"embedding", "rerank"} else "priority: low"
    return ["enhancement", "area: providers", priority] + [SCOPE_LABEL[c] for c in CAPS if c in finding.caps]


def provider_title(finding: ProviderFinding) -> str:
    return f"Provider candidate: {finding.name} {finding.marker}"


def provider_body(finding: ProviderFinding) -> str:
    compat = {True: "yes", False: "no", None: "unknown"}[finding.openai_compatible]
    lines = [
        f"{CATEGORY_TEXT[finding.category]}\n",
        "| | |", "|---|---|",
        f"| Catalog ids | {', '.join(f'{s}: `{p}`' for s, p in sorted(finding.ids.items()))} |",
        f"| API base URL | {f'`{finding.api}`' if finding.api else 'not listed'} |",
        f"| OpenAI-compatible per catalog | {compat} |",
    ]
    if finding.npm:
        lines.append(f"| AI SDK package | `{finding.npm}` |")
    if finding.doc:
        lines.append(f"| Docs | {finding.doc} |")
    status = "New in the catalogs." if finding.is_new else (
        "Already listed in the catalogs, but it just added embedding or rerank models."
    )
    return (
        "\n".join(lines) + f"\n\n{status}\n\n### Models ({len(finding.models)})\n\n"
        + _models_table(_finding_rows(finding))
        + "\n### Next steps\n\n" + NEXT_STEPS[finding.category] + FOOTER
    )


def provider_comment(finding: ProviderFinding, today: str) -> str:
    return f"### {today}: {len(finding.models)} new model(s)\n\n" + _models_table(_finding_rows(finding))


def rolling_title() -> str:
    return f"New models on supported providers {ROLLING_MARKER}"


def rolling_labels(changes: list[SupportedChange]) -> list[str]:
    caps = {c.cap for c in changes}
    return ["enhancement", "area: providers", "priority: low"] + [SCOPE_LABEL[c] for c in CAPS if c in caps]


def rolling_section(changes: list[SupportedChange], today: str) -> str:
    ready = [c for c in changes if not c.gap]
    gaps = [c for c in changes if c.gap]
    out = [f"### {today}\n"]
    if ready:
        out.append("**Config only.** Add a `[[providers.models]]` entry under a provider of this kind.\n")
        for kind in sorted({c.kind for c in ready}):
            rows = [(c.model, c.cap, ", ".join(sorted(c.sources))) for c in ready if c.kind == kind]
            out.append(f"#### `{kind}` ({len(rows)})\n\n" + _models_table(rows))
    if gaps:
        out.append(
            "**Capability gap.** The kind exists but does not serve this capability yet, so the model "
            "needs connector work (or, where the upstream is OpenAI-compatible, `kind = \"openai\"` with a `base_url`).\n"
        )
        for kind in sorted({c.kind for c in gaps}):
            rows = [
                (c.model, c.cap + (" (OpenAI-compatible upstream)" if c.upstream_compatible else ""),
                 ", ".join(sorted(c.sources)))
                for c in gaps if c.kind == kind
            ]
            out.append(f"#### `{kind}` ({len(rows)})\n\n" + _models_table(rows))
    return "\n".join(out)


def rolling_body(changes: list[SupportedChange], today: str) -> str:
    return (
        "Rolling list of new models on providers LUMEN already supports. Each watcher run adds a "
        "comment. Close this issue once the batch is triaged; the next run opens a fresh one.\n\n"
        + rolling_section(changes, today) + FOOTER
    )


# --- GitHub ------------------------------------------------------------------


class GitHub:
    def __init__(self, repo: str, token: str) -> None:
        self.repo = repo
        self.token = token

    def _call(self, method: str, path: str, payload: object | None = None) -> object:
        data = json.dumps(payload).encode() if payload is not None else None
        req = urllib.request.Request(
            f"https://api.github.com/repos/{self.repo}{path}", data=data, method=method,
            headers={
                "Authorization": f"Bearer {self.token}",
                "Accept": "application/vnd.github+json",
                "X-GitHub-Api-Version": "2022-11-28",
                "User-Agent": USER_AGENT,
                "Content-Type": "application/json",
            },
        )
        with urllib.request.urlopen(req, timeout=60) as resp:
            return json.loads(resp.read() or b"null")

    def watcher_issues(self) -> list[dict]:
        """Every issue (open or closed) the watcher filed, found by its title marker."""
        found, page = [], 1
        while True:
            batch = self._call("GET", f"/issues?state=all&per_page=100&page={page}")
            assert isinstance(batch, list)
            found += [i for i in batch if "pull_request" not in i and "[model-watch:" in i["title"]]
            if len(batch) < 100:
                return found
            page += 1

    def create(self, title: str, body: str, labels: list[str]) -> int:
        issue = self._call("POST", "/issues", {"title": title, "body": body, "labels": labels})
        assert isinstance(issue, dict)
        return int(issue["number"])

    def comment(self, number: int, body: str) -> None:
        self._call("POST", f"/issues/{number}/comments", {"body": body})

    def add_labels(self, number: int, labels: list[str]) -> None:
        self._call("POST", f"/issues/{number}/labels", {"labels": labels})


@dataclass
class Action:
    verb: str  # create | comment | skip
    title: str
    body: str
    labels: list[str]
    number: int | None = None
    reason: str = ""


def cap_body(text: str, limit: int = MAX_BODY) -> str:
    """Cut a body at a line boundary below `limit`, saying what was dropped."""
    if len(text) <= limit:
        return text
    note = (
        "\n\n_Truncated by the watcher: {n} more characters not shown. Run "
        "`python3 scripts/model-watch/model_watch.py --dry-run --show-bodies` for the full list._\n"
    )
    budget = limit - len(note.format(n=len(text)))
    cut = text.rfind("\n", 0, budget)
    cut = cut if cut > 0 else budget
    return text[:cut] + note.format(n=len(text) - cut)


def plan_actions(report: Report, existing: list[dict], today: str) -> list[Action]:
    def lookup(marker: str) -> tuple[dict | None, dict | None]:
        matches = [i for i in existing if marker in i["title"]]
        open_ = next((i for i in matches if i["state"] == "open"), None)
        closed = next((i for i in matches if i["state"] != "open"), None)
        return open_, closed

    actions: list[Action] = []
    for finding in report.providers:
        labels = provider_labels(finding)
        open_, closed = lookup(finding.marker)
        if open_:
            actions.append(Action("comment", open_["title"], provider_comment(finding, today), labels, open_["number"]))
        elif closed:
            actions.append(Action("skip", closed["title"], "", labels, closed["number"], "closed by a maintainer"))
        else:
            actions.append(Action("create", provider_title(finding), provider_body(finding), labels))
    if report.supported:
        labels = rolling_labels(report.supported)
        open_, _ = lookup(ROLLING_MARKER)
        if open_:
            actions.append(Action("comment", open_["title"], rolling_section(report.supported, today), labels, open_["number"]))
        else:
            actions.append(Action("create", rolling_title(), rolling_body(report.supported, today), labels))
    return actions


def apply_actions(gh: GitHub, actions: list[Action]) -> None:
    # Bodies are capped only here, on the way out, so `--dry-run --show-bodies`
    # still prints the full list the truncation note points to.
    for action in actions:
        if action.verb == "create":
            action.number = gh.create(action.title, cap_body(action.body), action.labels)
        elif action.verb == "comment" and action.number is not None:
            gh.comment(action.number, cap_body(action.body))
            gh.add_labels(action.number, action.labels)


# --- Snapshot ----------------------------------------------------------------


def read_snapshot(path: Path) -> dict[str, Catalog] | None:
    if not path.exists():
        return None
    data = json.loads(path.read_text(encoding="utf-8"))
    if data.get("version") != SNAPSHOT_VERSION:
        raise ShapeError(f"{path}: unsupported snapshot version {data.get('version')!r}")
    sources = data.get("sources")
    if not isinstance(sources, dict):
        raise ShapeError(f"{path}: 'sources' is not an object")
    return sources


def write_snapshot(path: Path, sources: dict[str, Catalog]) -> None:
    ordered = {
        name: {pid: dict(sorted(models.items())) for pid, models in sorted(cat.items()) if models}
        for name, cat in sorted(sources.items())
    }
    text = json.dumps({"version": SNAPSHOT_VERSION, "sources": ordered}, indent=1, ensure_ascii=False)
    path.write_text(clean(text) + "\n", encoding="utf-8")


def guard_shrink(old: dict[str, Catalog], fetched: Fetched) -> None:
    for name in list(fetched.catalogs):
        before, after = count(old.get(name, {})), count(fetched.catalogs[name])
        if before and after < before * MIN_RATIO:
            del fetched.catalogs[name]
            fetched.errors[name] = f"{name}: shrank from {before} to {after} models, refusing to record it"


# --- Output ------------------------------------------------------------------


def render_summary(report: Report, actions: list[Action], lumen: Lumen, dry_run: bool) -> str:
    out = ["## Model release watcher" + (" (dry run)" if dry_run else ""), ""]
    for name, err in sorted(report.source_errors.items()):
        out.append(f"- :warning: source `{name}` skipped: {err}")
    for warning in lumen.warnings:
        out.append(f"- :warning: {warning}")
    ready = sum(1 for c in report.supported if not c.gap)
    out += [
        f"- (a) config-only additions on supported providers: {ready}",
        f"- (a) capability gaps on supported providers: {len(report.supported) - ready}",
        f"- (b) OpenAI-compatible providers: {sum(1 for f in report.providers if f.category == 'b')}",
        f"- (c) providers needing a connector: {sum(1 for f in report.providers if f.category == 'c')}",
        f"- removed models (snapshot only): {', '.join(f'{k}: {v}' for k, v in sorted(report.removed.items()))}",
        "",
    ]
    if not actions:
        out.append("Nothing to file.")
    for action in actions:
        target = f"#{action.number}" if action.number else "new issue"
        if action.verb == "skip":
            out.append(f"- skip {target} `{action.title}` ({action.reason})")
        else:
            out.append(f"- {'would ' if dry_run else ''}{action.verb} {target}: `{action.title}` labels={action.labels}")
    return "\n".join(out) + "\n"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--dry-run", action="store_true", help="print the issues it would file; touch nothing")
    parser.add_argument("--init", action="store_true", help="write the snapshot from the current catalogs, file nothing")
    parser.add_argument("--snapshot", type=Path, default=HERE / "snapshot.json")
    parser.add_argument("--repo-root", type=Path, default=DEFAULT_REPO_ROOT)
    parser.add_argument("--source-dir", type=Path, help="read <source>.json files from here instead of fetching")
    parser.add_argument("--max-new-issues", type=int, default=10,
                        help="abort (filing nothing) if a run would open more issues than this")
    parser.add_argument("--show-bodies", action="store_true", help="with --dry-run, print full issue bodies")
    args = parser.parse_args(argv)
    if args.init and args.dry_run:
        print("error: --init writes the snapshot, --dry-run promises no writes; pick one", file=sys.stderr)
        return 1

    lumen = load_lumen(args.repo_root)
    fetched = fetch_all(args.source_dir)

    # --init never reads the old snapshot, so it can replace a corrupt or
    # older-version one.
    if args.init:
        if fetched.errors:
            print(f"error: cannot initialize with failing sources: {fetched.errors}", file=sys.stderr)
            return 1
        write_snapshot(args.snapshot, fetched.catalogs)
        print(f"wrote {args.snapshot} ({sum(count(c) for c in fetched.catalogs.values())} models)")
        return 0
    old = read_snapshot(args.snapshot)
    if old is None:
        print(f"error: {args.snapshot} not found; run with --init first", file=sys.stderr)
        return 1

    guard_shrink(old, fetched)
    report = diff_and_classify(old, fetched, lumen)
    today = dt.date.today().isoformat()

    token, repo = os.environ.get("GITHUB_TOKEN"), os.environ.get("GITHUB_REPOSITORY")
    gh = GitHub(repo, token) if token and repo else None
    if gh is None and not args.dry_run:
        print("error: GITHUB_TOKEN and GITHUB_REPOSITORY are required without --dry-run", file=sys.stderr)
        return 1
    existing = gh.watcher_issues() if gh else []
    actions = plan_actions(report, existing, today)

    summary = render_summary(report, actions, lumen, args.dry_run)
    print(summary)
    if args.dry_run and args.show_bodies:
        for action in actions:
            if action.verb != "skip":
                print(f"\n===== {action.verb}: {action.title}\n{action.body}")
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a", encoding="utf-8") as fh:
            fh.write(summary)

    creates = sum(1 for a in actions if a.verb == "create")
    if creates > args.max_new_issues:
        print(f"error: {creates} new issues exceeds --max-new-issues={args.max_new_issues}; a catalog likely "
              "renamed its ids. Nothing filed, snapshot unchanged. Inspect with --dry-run.", file=sys.stderr)
        return 1
    if args.dry_run:
        return 1 if report.source_errors else 0

    assert gh is not None
    apply_actions(gh, actions)
    # Only after every issue landed: a failed run leaves the old snapshot, so
    # the next run re-detects the same additions instead of losing them.
    merged = dict(old)
    merged.update(fetched.catalogs)  # failed sources keep their previous section
    write_snapshot(args.snapshot, merged)
    for name, err in sorted(report.source_errors.items()):
        print(f"::error::{err}")
    return 1 if report.source_errors else 0


if __name__ == "__main__":
    sys.exit(main())
