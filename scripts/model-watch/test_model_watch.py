"""Unit tests for model_watch.py. Run: python3 -m unittest discover scripts/model-watch"""

from __future__ import annotations

import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import model_watch as mw  # noqa: E402

REPO_ROOT = Path(__file__).resolve().parents[2]


def models_dev_provider(name, models, npm="@ai-sdk/openai-compatible", api="https://api.example.com/v1"):
    return {
        "id": name, "name": name.title(), "npm": npm, "api": api, "doc": "https://docs", "env": [],
        "models": {
            mid: ({"type": t} if t else {}) | {"modalities": {"output": ["text"]}}
            for mid, t in models.items()
        },
    }


def fetched(models_dev=None, litellm=None, openrouter=None, meta=None):
    catalogs = {}
    md_meta = {}
    if models_dev is not None:
        catalogs["models.dev"], md_meta = mw.parse_models_dev(models_dev)
    if litellm is not None:
        catalogs["litellm"] = mw.parse_litellm(litellm)
    if openrouter is not None:
        catalogs["openrouter"] = mw.parse_openrouter({"data": openrouter}, "chat")
    all_meta = {"models.dev": md_meta}
    if meta:
        all_meta.update(meta)
    return mw.Fetched(catalogs, all_meta, {})


class LumenParsingTest(unittest.TestCase):
    """The capability model is read from the real source tree."""

    def setUp(self):
        self.lumen = mw.load_lumen(REPO_ROOT)

    def test_every_kind_has_documented_capabilities(self):
        self.assertEqual(self.lumen.warnings, [])
        self.assertIn("openai", self.lumen.kinds)
        self.assertIn("vllm", self.lumen.kinds)

    def test_capabilities_follow_docs_and_kind_rs(self):
        caps = self.lumen.caps
        self.assertEqual(caps["anthropic"], {"chat"})
        self.assertEqual(caps["cohere"], {"chat", "embedding", "rerank"})
        self.assertNotIn("embedding", caps["groq"])  # supports_embeddings() is false
        self.assertIn("rerank", caps["together"])  # extra_capabilities in providers.json
        self.assertIn("groq", self.lumen.openai_compatible)
        self.assertNotIn("azure", self.lumen.openai_compatible)

    def test_resolve_by_name_alias_prefix_and_base_url(self):
        resolve = self.lumen.resolve
        self.assertEqual(resolve("openai"), "openai")
        self.assertEqual(resolve("together_ai"), "together")
        self.assertEqual(resolve("togetherai"), "together")
        self.assertEqual(resolve("amazon-bedrock"), "bedrock")
        self.assertEqual(resolve("vertex_ai-language-models"), "vertex_ai")
        self.assertEqual(resolve("someone", "https://api.groq.com/openai/v1/"), "groq")
        self.assertIsNone(resolve("brand-new-lab", "https://api.brand-new.ai/v1"))


class ParsingTest(unittest.TestCase):
    def test_models_dev_capabilities(self):
        data = {"p": models_dev_provider("p", {"chat-1": None, "emb-1": "embedding", "rr-1": "reranking", "img": "image"})}
        catalog, meta = mw.parse_models_dev(data)
        self.assertEqual(catalog["p"], {"chat-1": "chat", "emb-1": "embedding", "rr-1": "rerank"})
        self.assertTrue(meta["p"].openai_compatible)

    def test_models_dev_native_sdk_is_not_openai_compatible(self):
        _, meta = mw.parse_models_dev({"p": models_dev_provider("p", {"m": None}, npm="@ai-sdk/cohere", api=None)})
        self.assertFalse(meta["p"].openai_compatible)

    def test_models_dev_shape_change_is_rejected(self):
        with self.assertRaises(mw.ShapeError):
            mw.parse_models_dev({"p": {"name": "no models key"}})
        with self.assertRaises(mw.ShapeError):
            mw.parse_models_dev([1, 2])

    def test_litellm_modes(self):
        catalog = mw.parse_litellm({
            "sample_spec": {"litellm_provider": "x", "mode": "chat"},
            "a": {"litellm_provider": "acme", "mode": "chat"},
            "b": {"litellm_provider": "acme", "mode": "responses"},
            "c": {"litellm_provider": "acme", "mode": "rerank"},
            "d": {"litellm_provider": "acme", "mode": "image_generation"},
        })
        self.assertEqual(catalog, {"acme": {"a": "chat", "b": "chat", "c": "rerank"}})

    def test_openrouter_skips_image_only_outputs(self):
        catalog = mw.parse_openrouter({"data": [
            {"id": "x/text", "architecture": {"output_modalities": ["text"]}},
            {"id": "x/img", "architecture": {"output_modalities": ["image"]}},
        ]}, "chat")
        self.assertEqual(catalog, {"openrouter": {"x/text": "chat"}})
        with self.assertRaises(mw.ShapeError):
            mw.parse_openrouter({"models": []}, "chat")

    def test_em_dashes_are_scrubbed(self):
        catalog = mw.parse_litellm({"m\u2014x": {"litellm_provider": "acme", "mode": "chat"}})
        self.assertEqual(list(catalog["acme"]), ["m-x"])


class ClassificationTest(unittest.TestCase):
    def setUp(self):
        self.lumen = mw.load_lumen(REPO_ROOT)

    def classify(self, old, new):
        return mw.diff_and_classify(old, new, self.lumen)

    def test_new_model_on_supported_provider_is_config_only(self):
        old = {"models.dev": {"openai": {"gpt-a": "chat"}}}
        new = fetched(models_dev={"openai": models_dev_provider("openai", {"gpt-a": None, "gpt-b": None}, npm="@ai-sdk/openai", api=None)})
        report = self.classify(old, new)
        self.assertEqual([(c.kind, c.model, c.gap) for c in report.supported], [("openai", "gpt-b", False)])
        self.assertEqual(report.providers, [])

    def test_unsupported_capability_on_supported_kind_is_a_gap(self):
        old = {"models.dev": {"anthropic": {"claude": "chat"}}}
        new = fetched(models_dev={"anthropic": models_dev_provider("anthropic", {"claude": None, "claude-embed": "embedding"}, npm="@ai-sdk/anthropic", api=None)})
        report = self.classify(old, new)
        self.assertEqual([(c.model, c.gap) for c in report.supported], [("claude-embed", True)])

    def test_same_model_from_two_sources_is_listed_once(self):
        old = {"models.dev": {"openrouter": {}}, "openrouter": {"openrouter": {}}}
        new = fetched(
            models_dev={"openrouter": models_dev_provider("openrouter", {"v/m": None}, npm="@openrouter/ai-sdk-provider", api="https://openrouter.ai/api/v1")},
            openrouter=[{"id": "v/m", "architecture": {"output_modalities": ["text"]}}],
        )
        report = self.classify(old, new)
        self.assertEqual(len(report.supported), 1)
        self.assertEqual(report.supported[0].sources, {"models.dev", "openrouter"})

    def test_new_openai_compatible_provider_is_b(self):
        old = {"models.dev": {"openai": {"gpt-a": "chat"}}}
        new = fetched(models_dev={"newlab": models_dev_provider("newlab", {"m1": None, "e1": "embedding"})})
        [finding] = self.classify(old, new).providers
        self.assertEqual(finding.category, "b")
        self.assertTrue(finding.is_new)
        self.assertEqual(finding.api, "https://api.example.com/v1")
        self.assertIn("scope: embedding", mw.provider_labels(finding))
        self.assertIn("priority: medium", mw.provider_labels(finding))

    def test_new_native_provider_is_c(self):
        old = {"models.dev": {}}
        new = fetched(models_dev={"native": models_dev_provider("native", {"m1": None}, npm="native-sdk", api=None)})
        [finding] = self.classify(old, new).providers
        self.assertEqual(finding.category, "c")
        self.assertEqual(mw.provider_labels(finding), ["enhancement", "area: providers", "priority: low", "scope: chat"])

    def test_rerank_from_unsupported_provider_is_c_even_if_compatible(self):
        old = {"models.dev": {"lab": {"m1": "chat"}}}
        new = fetched(models_dev={"lab": models_dev_provider("lab", {"m1": None, "m2": None, "rr": "reranking"})})
        [finding] = self.classify(old, new).providers
        self.assertFalse(finding.is_new)
        self.assertEqual(finding.category, "c")
        # Chat additions on an already-known unsupported provider are baseline noise.
        self.assertEqual(set(finding.models), {("rr", "rerank")})

    def test_known_unsupported_provider_chat_only_is_silent(self):
        old = {"models.dev": {"lab": {"m1": "chat"}}}
        new = fetched(models_dev={"lab": models_dev_provider("lab", {"m1": None, "m2": None})})
        self.assertEqual(self.classify(old, new).providers, [])

    def test_provider_known_in_another_source_is_not_new(self):
        old = {"litellm": {"new_lab": {"x": "chat"}}}
        new = fetched(models_dev={"new-lab": models_dev_provider("new-lab", {"m1": None})})
        self.assertEqual(self.classify(old, new).providers, [])

    def test_litellm_provider_uses_openai_like_registry(self):
        old = {"litellm": {}}
        meta = {"litellm": {"acme": mw.ProviderMeta(name="acme", api="https://acme.ai/v1", openai_compatible=True)}}
        new = fetched(litellm={"m": {"litellm_provider": "acme", "mode": "chat"}}, meta=meta)
        [finding] = self.classify(old, new).providers
        self.assertEqual(finding.category, "b")


class PlanAndSnapshotTest(unittest.TestCase):
    def setUp(self):
        lumen = mw.load_lumen(REPO_ROOT)
        old = {"models.dev": {"openai": {"gpt-a": "chat"}}}
        new = fetched(models_dev={
            "openai": models_dev_provider("openai", {"gpt-a": None, "gpt-b": None}, npm="@ai-sdk/openai", api=None),
            "newlab": models_dev_provider("newlab", {"m1": None}),
        })
        self.report = mw.diff_and_classify(old, new, lumen)

    def test_creates_when_nothing_exists(self):
        actions = mw.plan_actions(self.report, [], "2026-10-31")
        self.assertEqual([a.verb for a in actions], ["create", "create"])
        self.assertIn("[model-watch:provider:newlab]", actions[0].title)
        self.assertIn(mw.ROLLING_MARKER, actions[1].title)
        for action in actions:
            self.assertNotIn(mw.EM_DASH, action.title + action.body)

    def test_comments_on_open_and_skips_closed(self):
        existing = [
            {"number": 7, "state": "closed", "title": "Provider candidate: Newlab [model-watch:provider:newlab]"},
            {"number": 3, "state": "closed", "title": f"old batch {mw.ROLLING_MARKER}"},
            {"number": 9, "state": "open", "title": f"New models on supported providers {mw.ROLLING_MARKER}"},
        ]
        actions = mw.plan_actions(self.report, existing, "2026-10-31")
        self.assertEqual([(a.verb, a.number) for a in actions], [("skip", 7), ("comment", 9)])
        self.assertIn("gpt-b", actions[1].body)

    def test_snapshot_round_trip_is_sorted_and_scrubbed(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "snapshot.json"
            mw.write_snapshot(path, {"s": {"b": {"z": "chat", "a": "chat"}, "a": {"m\u2014": "chat"}, "empty": {}}})
            text = path.read_text(encoding="utf-8")
            self.assertNotIn(mw.EM_DASH, text)
            data = json.loads(text)
            self.assertEqual(list(data["sources"]["s"]), ["a", "b"])
            self.assertEqual(list(data["sources"]["s"]["b"]), ["a", "z"])
            self.assertEqual(mw.read_snapshot(path), data["sources"])

    def test_shrinking_source_is_refused(self):
        old = {"openrouter": {"openrouter": {f"m{i}": "chat" for i in range(100)}}}
        new = mw.Fetched({"openrouter": {"openrouter": {"m1": "chat"}}}, {}, {})
        mw.guard_shrink(old, new)
        self.assertNotIn("openrouter", new.catalogs)
        self.assertIn("shrank", new.errors["openrouter"])

    def test_long_model_lists_are_capped(self):
        rows = [(f"m{i}", "chat", "x") for i in range(mw.MAX_LISTED + 5)]
        table = mw._models_table(rows)
        self.assertIn("... and 5 more", table)

    def test_oversized_bodies_are_capped_when_sent(self):
        # Many kinds at once can exceed GitHub's 65,536-character body limit
        # even with MAX_LISTED per table; a 422 would fail every later run.
        changes = [
            mw.SupportedChange(f"kind{k}", f"model-{k}-{i}-" + "x" * 40, "chat", {"models.dev"}, False)
            for k in range(40) for i in range(mw.MAX_LISTED)
        ]
        body = mw.rolling_body(changes, "2026-10-31")
        self.assertGreater(len(body), 65_536)

        sent = []

        class FakeGitHub:
            def create(self, title, text, labels):
                sent.append(text)
                return 1

            def comment(self, number, text):
                sent.append(text)

            def add_labels(self, number, labels):
                pass

        actions = [mw.Action("create", "t", body, []), mw.Action("comment", "t", body, [], 2)]
        mw.apply_actions(FakeGitHub(), actions)
        self.assertEqual(len(sent), 2)
        for text in sent:
            self.assertLessEqual(len(text), mw.MAX_BODY)
            self.assertIn("Truncated by the watcher", text)
        self.assertEqual(actions[0].body, body)  # dry-run output keeps the full list
        self.assertEqual(mw.cap_body("short"), "short")


class ReviewRegressionTest(unittest.TestCase):
    """Findings from the PR review: per-capability keys and CLI/snapshot edges."""

    def setUp(self):
        self.lumen = mw.load_lumen(REPO_ROOT)

    def test_unsupported_provider_keeps_each_capability_of_one_model_id(self):
        old = {"models.dev": {"lab": {"m0": "chat"}}, "litellm": {"lab": {"m0": "chat"}}}
        new = fetched(
            models_dev={"lab": models_dev_provider("lab", {"m0": None, "m": "embedding"})},
            litellm={"m0": {"litellm_provider": "lab", "mode": "chat"}, "m": {"litellm_provider": "lab", "mode": "rerank"}},
        )
        [finding] = mw.diff_and_classify(old, new, self.lumen).providers
        self.assertEqual(set(finding.models), {("m", "embedding"), ("m", "rerank")})
        self.assertEqual(finding.caps, {"embedding", "rerank"})
        self.assertEqual(finding.category, "c")
        self.assertIn("scope: reranking", mw.provider_labels(finding))

    def test_supported_kind_keeps_each_capability_with_its_own_gap(self):
        old = {"models.dev": {"anthropic": {}}, "litellm": {"anthropic": {}}}
        new = fetched(
            models_dev={"anthropic": models_dev_provider("anthropic", {"m": None}, npm="@ai-sdk/anthropic", api=None)},
            litellm={"m": {"litellm_provider": "anthropic", "mode": "embedding"}},
        )
        report = mw.diff_and_classify(old, new, self.lumen)
        self.assertEqual(
            sorted((c.model, c.cap, c.gap) for c in report.supported),
            [("m", "chat", False), ("m", "embedding", True)],
        )

    def test_read_snapshot_rejects_non_object_sources(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "snapshot.json"
            path.write_text(json.dumps({"version": mw.SNAPSHOT_VERSION, "sources": []}), encoding="utf-8")
            with self.assertRaises(mw.ShapeError):
                mw.read_snapshot(path)

    def test_init_and_dry_run_together_are_refused_without_writing(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "snapshot.json"
            self.assertEqual(mw.main(["--init", "--dry-run", "--snapshot", str(path)]), 1)
            self.assertFalse(path.exists())

    def test_init_replaces_an_unreadable_snapshot(self):
        catalogs = {"litellm": {"acme": {"m": "chat"}}}
        original = mw.fetch_all
        mw.fetch_all = lambda source_dir: mw.Fetched(catalogs, {}, {})
        try:
            with tempfile.TemporaryDirectory() as tmp:
                path = Path(tmp) / "snapshot.json"
                path.write_text('{"version": 999}', encoding="utf-8")
                self.assertEqual(mw.main(["--init", "--snapshot", str(path), "--repo-root", str(REPO_ROOT)]), 0)
                self.assertEqual(mw.read_snapshot(path), catalogs)
        finally:
            mw.fetch_all = original


if __name__ == "__main__":
    unittest.main()
