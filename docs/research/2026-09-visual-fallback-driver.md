# Visual fallback driver — AX-first, vision-fallback design

**Status**: research / design (no code). 2026-09.
**Scope**: what to build when the accessibility tree is unusable; nothing here
changes the existing AX / snapshot-token / guarded-batch contract, which stays
the primary and preferred channel.

## 中文摘要

iPhone 自动化目前完全依赖 WDA 的可访问性树（`/agent/elements`），但游戏、canvas
应用、自绘 UI 会返回近乎空的树，而 KakaoTalk 这类超大树甚至会直接杀死手机上的
WDA runner（issue #44）。本文设计一条可选的视觉降级通道：检测到 AX 树不可用时，
改用「截图 → 视觉模型 → 归一化坐标动作」的循环，复用现有的 `tap/longpress/swipe`
坐标动作。关键发现是：坐标动作走 W3C `/actions`、健康探测走 `/wda/apps/list`，
两者都不解析 AX 层级，所以纯视觉循环天然绕开了 KakaoTalk 崩溃。推荐由 MCP/skill
层编排（调用方自己就是视觉模型），daemon 只需增加只读的 `ax_stats` 统计字段，
保持「薄而可靠」的哲学。结论：可行，且第一版几乎不需要 daemon 改动。

---

## 0. The two failure modes (and why one design covers both)

Everything below is grounded in two documented, opposite failure modes:

**Mode A — the tree is too sparse.** Games (Unity/Unreal), canvas/WebGL apps,
and custom-drawn UI expose almost nothing to XCUITest. The repo has seen
1-element trees where the only row is the `Application` node. The
`/agent/elements` call *succeeds*, but the rows carry no actionable targets:
no labels, no interactive kinds, rects covering a sliver of the screen.

**Mode B — the tree is too large to read at all.**
[Issue #44](https://github.com/leeguooooo/iphone-use/issues/44) (KakaoTalk,
reproduced 3/3 on daemon 0.5.3): the moment KakaoTalk is foreground, *any*
request that resolves the accessibility hierarchy kills the on-phone runner —
`wda-runner.log` shows `Requesting snapshot of accessibility hierarchy for app
with pid …` followed by `** BUILD INTERRUPTED **`. `/agent/elements` hangs
past 25–40 s, the next action returns `outcome_unknown`, and status flips to
`device_state:"blocked"`. Recovery costs a 1–3 min WDA rebuild, then it dies
again on the next AX read. The same session drove Safari (91 elements) fine.

Mode A means "AX gave you nothing to act on"; Mode B means "asking AX at all
takes the device down". A vision channel that acts on **coordinates only** and
**never resolves the hierarchy** handles both.

### The load-bearing code fact

Verified in the current source (`crates/server/src/wda.rs`):

- Coordinate taps, long-presses, drags and swipes are dispatched through the
  W3C Actions API — `POST /session/<sid>/actions` (`tap_point` at
  `wda.rs:366`, with the same pattern for longpress/drag/swipe). **No AX
  hierarchy snapshot is taken.**
- The action-level health probe (`probe_health`, `wda.rs:91`) uses
  `GET /wda/apps/list` + lock state — also snapshot-free (the comment at
  `wda.rs:84` explicitly avoids `wda/activeAppInfo` for a related reason).
- `GET /agent/screenshot` is a pixel capture; no hierarchy resolution.
- `POST /agent/input` with `{"type":"tap","x":…,"y":…}` reads the element tree
  **only** when `?return=delta` is requested (`http.rs:5411` — delta is read
  only after `want_delta && outcome == Applied`).

So the snapshot-free action channel the vision driver needs **already exists
end to end**: screenshot → normalized-coordinate action → screenshot. The
missing pieces are (1) deciding *when* to switch, (2) the model contract, and
(3) a discipline that keeps implicit AX reads (`?return=delta`, element
verification) out of the loop while a Mode-B app is foreground.

---

## 1. "Is the AX tree usable?" — detection signals

### 1.1 Signals computable from the existing `/agent/elements` response

The response is `{screen:{width,height}, snapshot, elements:[{kind, label,
identifier?, rect:[x,y,w,h], depth, value?, enabled?, visible?, …}]}`. The
daemon already filters rows to "non-empty label OR interactive kind"
(`flatten_tree`, `wda.rs:974`, with the 11-kind `INTERACTIVE` list: Button,
Cell, TextField, SecureTextField, SearchField, Switch, Slider, TextView,
PickerWheel, Picker, Stepper). All of the following are pure functions of data
already serialized:

| Signal | Definition | Healthy screen (Safari, Home) | Mode-A screen (game/canvas) |
|---|---|---|---|
| `n` — element count | `elements.len()` | tens to ~200 | 0–3 |
| `n_interactive` | rows whose `kind` ∈ INTERACTIVE | > 5 typical | 0 |
| `labeled_frac` | interactive rows with non-empty `label` ÷ `n_interactive` (1.0 if none) | ≥ 0.6 | n/a (no interactive rows) |
| `coverage` | area of the union of rects (clipped to `screen`) ÷ screen area | ≥ 0.5 | ≈ 0 or a full-screen single `Other`/`Application` rect |
| `container_only` | every row's `kind` ∈ {Application, Window, Other, Image} | false | true |
| `max_depth` | max of `depth` | ≥ 4 | ≤ 2 |

Caveats worth encoding:

- A **single full-screen rect** (the app's root `Other` at depth 1) makes
  `coverage` ≈ 1.0 while being useless — that's why `container_only` and
  `n_interactive` must gate coverage, not the other way round.
- **Legitimately sparse screens exist**: a full-screen video player or photo
  viewer has few elements but the few it has (a Button labeled "Done") are
  correct. Low `n` alone must not trigger fallback; *zero usable targets for
  the current intent* should.
- **Mode B produces no response to score.** The signal is the read outcome
  itself: `/agent/elements` returning 504 `wda_source_timeout` / 502
  `wda_source_failed` (with `transitioning:true`) while `GET /agent/screenshot`
  still succeeds, twice in a row on the same foreground app. After issue #44's
  requested fix (bounded snapshot timeout that fails the request instead of
  the runner), this becomes a clean, cheap signal; today it is expensive
  (runner rebuild), which argues for **remembering the verdict per app** so
  the probe is paid at most once per app per session.

### 1.2 Proposed scoring heuristic

Score in three tiers rather than a single scalar — the actions differ:

```
ax_verdict(elements_response | read_failure) -> usable | degraded | unusable

unusable  (go vision):
    read failed twice (Mode B: elements 502/504 while screenshot 200), OR
    n_interactive == 0 AND container_only                        (Mode A)

degraded  (hybrid: AX for what it has, vision for the rest):
    n_interactive < 3, OR labeled_frac < 0.3, OR
    (coverage < 0.3 AND NOT container_only)

usable    (stay AX-only; today's behavior):
    everything else
```

Thresholds are starting points to calibrate against stored trees (a Safari
tree, a Home-screen tree, a known game tree); the shape — a
zero-interactive-targets test plus a read-failure test, with a hybrid middle
band — matters more than the exact constants.

### 1.3 Where it lives: stats in the daemon, policy in the skill/client

Split it:

- **Daemon: computes and reports, never decides.** Add an additive, optional
  `ax_stats` object to the `/agent/elements` response:
  `{"n":…, "n_interactive":…, "labeled_frac":…, "coverage":…,
  "container_only":…, "max_depth":…}`. This is a pure function of rows it
  already serializes (a sibling of `elements_delta_json`), costs microseconds,
  cannot regress any existing client (new key, same response otherwise), and
  gives every client — skill, MCP, browser drawer, future flows — one shared,
  unit-tested implementation instead of N divergent ones.
- **Skill/MCP layer: applies thresholds and owns the switch.** Whether to go
  vision is *policy* — it depends on task intent ("read the score" vs "tap the
  button"), cost tolerance, and model availability, none of which the daemon
  knows. The skill doc gets a short "when the tree is unusable" section citing
  `ax_stats`; the MCP server can surface a convenience verdict.
- The Mode-B signal (read failed, screenshot works) needs no new code at all —
  both statuses are already distinguishable by the client today.

This respects the repo's thin-daemon philosophy: the daemon stays a
deterministic device driver; judgment stays with the model that has the task
context.

---

## 2. Visual driver interface

### 2.1 The contract

Provider-agnostic, defined so the model behind it is swappable. Input is what
the daemon already produces; output is one of the **existing** action JSONs —
no new action types, so the daemon needs zero changes to execute a
vision-driven step.

```jsonc
// VisualGroundingRequest
{
  "screenshot_png_b64": "…",           // from GET /agent/screenshot, verbatim
  "screen": {"width": 393, "height": 852},  // points, from /agent/elements
                                            // or screenshot metadata
  "instruction": "tap the paperclip / attach button in the message bar",
  "history": [                          // optional, last k steps for loops
    {"action": {…}, "observed": "keyboard appeared"}
  ],
  "hints": {                            // optional, hybrid mode (§1.2 degraded)
    "known_elements": [ /* the few usable ElementRows, as-is */ ]
  }
}

// VisualGroundingResponse
{
  "action": {"type": "tap", "x": 0.708, "y": 0.089},  // EXISTING schema:
      // tap | longpress (+duration_ms) | swipe/scroll (x,y,dx,dy) — normalized [0,1]
  "confidence": 0.86,                  // 0..1, calibration per provider
  "reasoning": "attach icon is the leftmost glyph in the input bar",
  "target_description": "paperclip icon, bottom-left of message field",
  "abstain": false                     // true => no action field; see §4
}
```

As Rust, for whenever a native integration is wanted (not v1):

```rust
#[async_trait]
trait VisualGrounder {
    async fn ground(&self, req: GroundingRequest) -> Result<GroundingResponse>;
}
// impls: CallingAgent (implicit, see below), OpenAiCompatible { base_url, model },
// future: local vLLM / CoreML.
```

An OpenAI-compatible chat-completions endpoint is the pragmatic wire format —
UI-TARS ships served by vLLM/SGLang exactly that way, and every hosted VLM
speaks it; the trait keeps the daemon-side door open without committing to it.

### 2.2 Who calls the model: skill/MCP orchestration, not the daemon

**Recommendation: the MCP/skill layer orchestrates** (screenshot → model →
coordinate action). The daemon does not gain a model client, API keys, or new
config in v1. Reasons, in order of weight:

1. **The thin-daemon philosophy is load-bearing here.** The daemon's value is
   deterministic device control with a strict outcome grammar
   (`outcome`/`retry_safe`, at-most-once mutation, guarded batches). A model
   call is the opposite kind of dependency: slow (seconds), nondeterministic,
   networked, key-managed, and versioned. Putting it inside `agent_input`
   would couple WDA-lock hold times to model latency and turn model outages
   into device-state errors.
2. **The calling agent already has everything.** It receives screenshots, has
   the task context (which the daemon never has — "tap the attach button" is
   not expressible in the daemon's vocabulary), and can already emit
   `{"type":"tap","x":…,"y":…}`. For the default provider (§3.4) the "call"
   is not even an HTTP request — it's the agent reasoning over the screenshot
   it just fetched.
3. **Privacy stays a user choice.** Screenshots of banking/IM apps leaving the
   Mac should be decided by whoever configures the orchestrator, not baked
   into the device daemon. A daemon that phones a vision API is a much bigger
   trust surface than one that serves pixels to an authenticated local client.
4. **Swappability is free at the skill layer.** Pointing the skill/MCP at a
   local UI-TARS vLLM endpoint vs. using the agent's own eyes is a
   configuration of the orchestrator; redeploying the daemon for a model swap
   would be backwards.

What the daemon *should* add (all additive, all optional):

- `ax_stats` on `/agent/elements` (§1.3).
- A documented **"vision-safe" discipline**: while driving a Mode-B app, the
  client must not send `?return=delta`, element/label taps, or `wait_for`
  element gates — anything that resolves the hierarchy. This is documentation
  plus perhaps a skill-level lint, not daemon enforcement, in v1. (A future
  `?no_ax=1` hint that makes the daemon refuse accidental AX reads for the
  request would be a small, honest guard.)
- Optionally, cheap screenshot metadata to help verification (§4): e.g.
  `X-Screenshot-Hash` header (perceptual hash) so a client can detect "screen
  changed" without re-downloading identical PNGs. Nice-to-have, not required.

Env-style config, if/when a daemon-side provider is ever wanted, follows the
existing convention: `PHONE_REMOTE_VISION_URL`, `PHONE_REMOTE_VISION_TOKEN`,
`PHONE_REMOTE_VISION_MODEL`. Explicitly **out of scope for v1**.

---

## 3. Model options (researched 2026-09; verify before building)

### 3.1 Apple Ferret-UI Lite

- 3B end-to-end GUI agent for mobile/web/desktop; paper Sep 2025
  ([arXiv 2509.26539](https://arxiv.org/abs/2509.26539)), publicized wider
  Feb 2026 ([Apple ML Research](https://machinelearning.apple.com/research/ferret-ui),
  [InfoQ](https://www.infoq.com/news/2026/02/apple-ferret-ui-lite-on-device/)).
- Grounding: **91.6% ScreenSpot-V2, 53.3% ScreenSpot-Pro, 61.2% OSWorld-G** —
  competitive with far larger models at 3B, sized for on-device.
- **Blocker: licensing.** The lineage ships under research-only terms — the
  Ferret family repo ([apple/ml-ferret](https://github.com/apple/ml-ferret))
  and the Ferret-UI data are CC BY-NC / research-use; nothing indicates a
  commercially usable checkpoint, and no OS-integrated API exists yet. For a
  shipping tool, **not usable today**; watch for it surfacing as an Apple
  Intelligence system capability instead.

### 3.2 ByteDance UI-TARS

- The one **actually deployable open** option:
  [UI-TARS-1.5-7B](https://huggingface.co/ByteDance-Seed/UI-TARS-1.5-7B),
  **Apache-2.0**, weights on Hugging Face, served via vLLM or SGLang
  (`vllm serve ByteDance-Seed/UI-TARS-1.5-7B`). Model card:
  **94.2% ScreenSpot-V2, 61.6% ScreenSpot-Pro, 64.2 AndroidWorld** — the
  strongest published mobile grounding among open checkpoints.
- [UI-TARS-2](https://arxiv.org/abs/2509.02544) (Sep 2025) improves further,
  but as of this research **UI-TARS-2 weights are not on Hugging Face**
  (open [request issue #213](https://github.com/bytedance/UI-TARS/issues/213));
  it's paper + hosted demo. Plan around 1.5-7B.
- Serving cost: a 7B VLM wants a real GPU (≥ ~24 GB VRAM comfortable for
  bf16; quantized GGUF variants exist for less). On the user's Apple-Silicon
  Macs this means either a quantized local run (slower, seconds/step) or a
  small GPU box/cloud endpoint. Latency per grounding call on a served GPU:
  roughly sub-second to ~2 s; local quantized: several seconds.
- Privacy: self-hostable — screenshots never leave the LAN. This is its main
  argument beside cost-per-step.

### 3.3 Generic multimodal LLMs (Claude / GPT / Gemini via API)

- Grounding on **dense desktop** UIs is historically the weak spot —
  [ScreenSpot-Pro](https://huggingface.co/blog/Ziyang/screenspot-pro) measured
  GPT-4o at **0.8%** vs. specialist models' 40–60%. But ScreenSpot-Pro is 4K+
  professional desktop screens with tiny targets; it is the *wrong* benchmark
  for this project.
- The relevant regime is a **phone screen**: ~393×852 points, Apple-HIG
  ≥ 44 pt touch targets, one column of content. That's ScreenSpot-V2-mobile
  territory, where even mid-tier models score high and a normalized-coordinate
  error of ±0.03 still lands inside the target. Frontier models with native
  computer-use/pointing training (Claude's computer-use models, Gemini 2.5+)
  are markedly better than the GPT-4o datapoint above, and — decisively —
  the **calling agent already has the screenshot in context**, so the marginal
  grounding call costs zero extra infrastructure, zero extra latency beyond
  the reasoning it was doing anyway, and inherits the task context.
- Cost: a screenshot + short prompt per step against a frontier API is the
  most expensive per-step option in tokens, but it's only paid on fallback
  screens, and the skill's existing "vision once → script forever" doctrine
  (compile successful traces into flows) bounds the recurring cost.

### 3.4 The honest comparison, including "do nothing in the daemon"

| Option | Mobile grounding | Latency/step | Cost/step | Privacy | Deployable today |
|---|---|---|---|---|---|
| Calling agent (Claude/GPT) as grounder | good on phone-sized UI; weakest on dense/tiny targets | already in the loop | frontier tokens (fallback screens only) | screenshots already go to the agent's API today | **yes — zero new infra** |
| UI-TARS-1.5-7B, self-hosted | best-in-class open (94.2 SS-V2) | ~1–2 s served; more if local-quantized | ~zero marginal after GPU | LAN-only possible | yes, needs a GPU endpoint |
| Ferret-UI Lite | strong at 3B, designed on-device | would be great | — | best (on-device) | **no — research license** |
| UI-TARS-2 | best published | — | — | — | no open weights yet |

**Does a dedicated grounding model beat the calling agent today?** On
benchmarks, yes — UI-TARS-1.5-7B out-grounds generic frontier models,
decisively so on dense screens. **For this project, mostly no, not yet**: the
fallback triggers on games/canvas/AX-broken apps shown on a *phone-sized*
screen, where frontier-model grounding plus large touch targets is usually
sufficient; the calling agent needs no GPU, no serving, no new privacy
surface, and it already holds the task intent. The dedicated model becomes
worth its GPU when (a) vision steps become frequent enough that token cost
dominates, (b) targets get small (dense game HUDs, drawing apps), or (c) the
user wants screenshots to stay on the LAN. The provider-agnostic contract in
§2.1 is exactly what makes that a later config change instead of a redesign.

**Recommendation: v1 = "do nothing in the daemon."** The skill/MCP layer
implements the loop with the calling agent as the grounder; the §2.1 JSON is
the *format the skill instructs the agent to emit*, so swapping in UI-TARS
later is transparent.

---

## 4. Degradation strategy

### 4.1 Low confidence / abstention

- `confidence < 0.5` or `abstain:true` → **no dispatch**. Nothing was sent, so
  this is by definition `outcome:"not_sent"`-equivalent, `retry_safe:true` at
  the orchestrator level. Recovery ladder, in order: re-screenshot (the screen
  may have settled/animated); ask the model again with a **cropped region**
  around its best guess (cheap resolution boost — crop client-side, no daemon
  change); switch strategy (scroll to bring the target into view); finally
  report to the user with the screenshot and the model's
  `target_description`.
- Confidence never overrides the existing safety rules: destructive targets
  (send / pay / delete / 2FA) keep requiring explicit verification regardless
  of channel; a vision guess is *weaker* evidence than an AX label, never
  stronger.

### 4.2 Verifying a vision-driven tap landed

- **Post-action screenshot diff is the primary verifier** in vision mode:
  screenshot before, act, settle ~300–800 ms, screenshot after, compare
  (pixel-diff fraction or perceptual hash + the model judging "did the
  expected change happen?" on the after-image). The model-judged variant
  doubles as re-orientation for the next step, so it costs one call, not two.
- **`?return=delta` is explicitly forbidden in Mode B** — it resolves the
  element tree after the action (`http.rs:5411`), which is precisely the
  operation that kills the KakaoTalk runner. This is the sharpest correctness
  edge in the whole design: the vision loop must be *hermetically* AX-free
  for Mode-B apps. In Mode A (sparse but harmless tree), `return=delta` is
  safe but usually uninformative — a game's tree doesn't change when you tap;
  screenshot diff is the verifier there too. `return=delta` stays exactly
  what it is today for the AX channel; no regression.
- An unchanged screen after an applied tap is a *soft* failure: the tap may
  have landed on dead space, or the UI may respond invisibly. One retry with
  an adjusted target (model sees before+after and its own previous guess) is
  reasonable; more than one is guessing — stop and report.

### 4.3 Mapping onto the existing outcome / retry_safe taxonomy

The daemon's grammar is untouched; the orchestrator layers vision semantics
on top of it:

| Situation | Daemon says | Orchestrator treats as |
|---|---|---|
| Model abstained / low confidence | (nothing sent) | `not_sent`, retry-safe, try recovery ladder |
| Tap dispatched, applied | `ok:true` / `outcome:applied` | *dispatched*, not *achieved* — screenshot diff decides success |
| Tap dispatched, `outcome_unknown` (502/504) | `retry_safe:false` | same rule as today: **read state (screenshot) before any replay**; vision mode already re-screenshots every step, so this integrates naturally |
| `wda_pre_dispatch_failed` / transition | `not_sent`, `retry_safe:true` | safe to retry after status settles |
| Applied but screen unchanged | `applied` | vision-level soft failure: one adjusted retry, then stop |

Note the pleasant alignment: the skill already mandates "treat
`outcome_unknown` as possibly executed; read a screenshot before resending."
The vision loop makes that mandatory read structural rather than
disciplinary.

### 4.4 Session-level degradation

- Cache the per-app verdict (`usable | degraded | unusable`, keyed by the
  foreground app when known, else by screen signature) for the session, so
  Mode-B apps pay the probing cost at most once.
- Any successful AX read on a new screen flips the channel back — AX-first is
  the invariant; vision is a screen-scoped fallback, not a session mode.
- Every successful vision-driven sequence should feed the existing
  "vision once → script forever" pipeline: coordinates get compiled into a
  flow bound to a screen signature + postcondition, per the skill's existing
  coordinate-fallback rules (SKILL.md §Self-improvement). Vision is how you
  *discover* the flow, not how you run it the tenth time.

---

## Feasibility verdict

**Feasible, and cheaper than expected.** The critical enabler is already
shipped: coordinate actions go through W3C `/actions` and the health probe
through `/wda/apps/list`, neither of which resolves the accessibility
hierarchy — so a screenshot → VLM → normalized-coordinate loop works against
today's daemon with **zero daemon changes**, and it is simultaneously the
workaround for issue #44's runner-killing apps (provided the loop is kept
strictly AX-free: no `/agent/elements`, no `?return=delta`, no element
locators while such an app is foreground). The recommended v1 is skill/MCP
orchestration with the calling agent as the grounder; the only daemon work
worth doing is additive (`ax_stats` on `/agent/elements`, and the issue-#44
bounded-snapshot fix, which is independently owed). A dedicated grounding
model (UI-TARS-1.5-7B, Apache-2.0, vLLM-servable) is the clear upgrade path
when cost, density, or privacy demand it; Ferret-UI Lite is license-blocked;
UI-TARS-2 has no open weights yet.

## Next-step spike (offline, no phone, no daemon changes)

**Goal**: prove the contract end to end on stored pixels.

1. Take 3 stored screenshots: one AX-empty app (a game or canvas app; if none
   stored, any full-screen Unity game screenshot at iPhone resolution), one
   KakaoTalk-like chat screen, one healthy control (Safari).
2. For each, run one generic VLM (the calling agent itself — no new infra)
   with the §2.1 prompt contract and a concrete instruction ("tap the
   settings gear", "tap the attach button"), requiring the
   `{action, confidence, reasoning, target_description}` JSON.
3. Validate: (a) output parses as an existing `/agent/input` action schema
   with in-range normalized coordinates; (b) overlay the predicted point on
   the screenshot and eyeball whether it lands inside the true target;
   (c) record per-screen hit/miss and confidence calibration.
4. Deliverable: a scratch script + a table of hits/misses appended to this
   doc.

**Effort**: ~half a day. **Hardware follow-up** (needs the phone, separate
session): verify on-device that a strictly AX-free loop (status → screenshot →
coordinate tap → screenshot) survives inside KakaoTalk without killing the
runner — that single experiment confirms both the issue-#44 workaround and
the vision channel's viability, ~1–2 hours.

## Sources

- [Issue #44 — KakaoTalk a11y snapshot kills the WDA runner](https://github.com/leeguooooo/iphone-use/issues/44)
- [UI-TARS-1.5-7B model card (benchmarks, Apache-2.0, vLLM serving)](https://huggingface.co/ByteDance-Seed/UI-TARS-1.5-7B)
- [UI-TARS GitHub](https://github.com/bytedance/ui-tars) · [UI-TARS-2 technical report](https://arxiv.org/abs/2509.02544) · [UI-TARS-2 weights request #213](https://github.com/bytedance/UI-TARS/issues/213)
- [Ferret-UI Lite paper](https://arxiv.org/abs/2509.26539) · [Apple ML Research page](https://machinelearning.apple.com/research/ferret-ui) · [apple/ml-ferret (research license)](https://github.com/apple/ml-ferret) · [InfoQ coverage](https://www.infoq.com/news/2026/02/apple-ferret-ui-lite-on-device/)
- [ScreenSpot-Pro benchmark](https://github.com/likaixin2000/ScreenSpot-Pro-GUI-Grounding) · [ScreenSpot-Pro results blog (GPT-4o 0.8%)](https://huggingface.co/blog/Ziyang/screenspot-pro)
