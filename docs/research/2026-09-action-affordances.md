# Action affordances as first-class element data (`/agent/elements` + a generic `perform` action)

Research doc, 2026-09. Investigates what WebDriverAgent (WDA) can expose per element
beyond what `crates/server/src/wda.rs` parses today, and designs an additive, sparse
way to surface each element's *available actions* plus a generic action to invoke them
— inspired by the LUMOS pattern ([arXiv 2606.30697](https://arxiv.org/abs/2606.30697):
accessibility metadata → machine-readable "semantic blueprints" with roles, values,
bounds, **and action affordances**) and macOS open-computer-use's
`perform_secondary_action` (macOS AX exposes `AXActionNames` per element; iOS does not,
so affordances must be *derived*).

All WDA findings below were verified against the actual pinned source
(appium/WebDriverAgent **v9.15.3** tarball), not documentation. File references are to
that tree.

## 中文摘要

调研结论：本仓库当前只解析了 WDA `/source?format=json` 每个节点字段的一个子集，而 v9.15.3
的响应里**本来就带着** `traits`（无障碍特征串，含 `Adjustable`/`Selected`/`ToggleButton` 等）、
`minValue`/`maxValue`（Slider/Stepper 专有）等字段——也就是说暴露"该元素支持哪些动作"
不需要改 WDA、不增加任何设备端开销，只需在 `flatten_tree` 里多解析几个已经在 JSON 里的键。
调用侧 WDA 已有成套的 element-scoped 手势路由（`/wda/element/:uuid/touchAndHold、doubleTap、
pinch、rotate、forceTouch、scroll` 及 `/wda/pickerwheel/:uuid/select` 的 next/previous），
可以承载一个通用的 `{"type":"perform","element":N,"snapshot":"…","action":"…"}` 动作，
完全复用现有 snapshot-token 防陈旧机制和 fail-closed 错误分类。唯一拿不到的是
`UIAccessibilityCustomAction`（自定义动作列表）：XCUITest 公开层和 WDA v9.15.3 源码里
完全没有这条通路，需要 fork 打补丁且结果不确定。总体判定：**高度可行，核心零 fork**；
建议先做一个 env-flag 后置的 traits/min/max 解析 spike（约半天到一天）。

---

## 1. Inventory: what WDA v9.15.3 actually exposes

### 1.1 `/source?format=json` — per-node keys (the tree the daemon already fetches)

Serializer: `XCUIApplication+FBHelpers.m`, `dictionaryForElement:recursive:excludedAttributes:`
plus `fb_attributeBlockMapForWrappedSnapshot:`.

Always emitted per node:

| key | notes | parsed by `flatten_tree` today? |
|---|---|---|
| `type` | `XCUIElementType…` short name | yes (`kind`) |
| `rawIdentifier` | accessibility identifier or null | yes (`identifier`) |
| `name`, `label`, `value` | | yes |
| `rect` | `{x,y,width,height}` dict | yes |
| `frame` | `NSStringFromCGRect` **string** (`"{{x, y}, {w, h}}"`), infinite values clamped | no (redundant with `rect`) |
| `nativeFrame` | raw rendered frame, string | no |
| `isEnabled` / `isVisible` / `isAccessible` / `isFocused` | `"0"`/`"1"` strings | yes |
| **`traits`** | **comma-separated accessibility-trait names, e.g. `"Button, Selected"`** | **no — the key finding** |

Conditionally emitted:

| key | condition (`FBElementHelpers.m`) | parsed today? |
|---|---|---|
| `placeholderValue` | only TextView / TextField / SearchField / SecureTextField | yes |
| **`minValue`**, **`maxValue`** | **only Slider and Stepper** (`FBDoesElementSupportMinMaxValue`), emitted as `NSNumber.stringValue` | **no** |

So **every `/source` response the daemon already pays for carries `traits` and
(for sliders/steppers) `minValue`/`maxValue`; the daemon currently throws them away.**
Parsing them costs zero extra WDA traffic and zero extra snapshot time.

`GET /source` also accepts `excluded_attributes=<comma-list>` (`FBDebugCommands.m:49`) —
we could *exclude* `traits` for payload thrift, but not request anything extra; the set
above is the ceiling for the JSON tree.

### 1.2 The traits vocabulary (`FBAccessibilityTraits.m`)

`traits` is the element's `UIAccessibilityTraits` bitmask rendered to names:

`None, Button, Link, Header, SearchField, Image, Selected, PlaysSound, KeyboardKey,
StaticText, SummaryElement, NotEnabled, UpdatesFrequently, StartsMediaSession,
Adjustable, AllowsDirectInteraction, CausesPageTurn, TabBar` — plus, when WDA is built
with Xcode 15+/clang 16 on iOS 17+ (true for this repo's local builds):
`ToggleButton, SupportsZoom`.

Affordance-relevant signal:

- **`Adjustable`** — the element implements `accessibilityIncrement`/`accessibilityDecrement`.
  This is the iOS-native "increment/decrement affordance" bit. It appears on sliders,
  steppers, pickers, and **custom SwiftUI/UIKit adjustable controls that the element
  tree otherwise shows as `Other`** — exactly the rows an agent currently can't
  recognize as drivable.
- **`Selected`** — selection state (tab bars, segmented controls, filter chips).
  Today invisible in `/agent/elements`; `value` is usually empty for these.
- **`ToggleButton`** (iOS 17+) — a button with on/off semantics that is *not* a
  `Switch` in the tree.
- **`Link`, `Header`, `TabBar`, `SummaryElement`** — structural semantics (useful for
  locators, not actions).
- `NotEnabled` duplicates `isEnabled:false`; `StaticText`/`Image`/`Button`/`SearchField`/
  `KeyboardKey` mostly duplicate `type`. These should be filtered out, not re-emitted.

### 1.3 What the tree does NOT carry (but per-element routes do)

Available only via `GET /session/:sid/element/:uuid/attribute/:name` (any `wd*`
property in `Routing/FBElement.h`) or a dedicated route, i.e. one HTTP round-trip
per element after a find:

- `selected` (`GET /element/:uuid/selected`) — but redundant: the `Selected` trait in
  the tree carries the same bit for free.
- `hittable` (`wdHittable`) — genuine "can a synthesized touch land here" signal;
  **not** in the JSON tree at v9.15.3 (upstream has an `includeHittableInPageSource`
  setting only in newer majors' XML path). N+1 cost — not worth bulk use.
- `index` (`wdIndex`), `accessibilityContainer`.

### 1.4 Per-element action routes (`FBElementCommands.m`, non-tvOS)

Element-scoped (take an element UUID from a find; WDA computes the geometry — no
rect math on our side, immune to the stale-rect problem `tap_snapshot_element`
already defends against):

| route | parameters | affordance |
|---|---|---|
| `POST /wda/element/:uuid/touchAndHold` | `duration` (s) | long-press → context menu ("secondary action") |
| `POST /wda/element/:uuid/doubleTap` | — | double tap |
| `POST /wda/element/:uuid/twoFingerTap` | — | two-finger tap |
| `POST /wda/element/:uuid/tapWithNumberOfTaps` | `numberOfTaps`, `numberOfTouches` | n-tap/n-touch |
| `POST /wda/element/:uuid/forceTouch` | `pressure?`, `duration?` | force press (peek/pop-era; niche) |
| `POST /wda/element/:uuid/pinch` | `scale`, `velocity` | pinch zoom in/out |
| `POST /wda/element/:uuid/rotate` | `rotation`, `velocity` | rotate |
| `POST /wda/element/:uuid/swipe` | `direction` (`up/down/left/right`), `velocity?` | directional swipe inside the element |
| `POST /wda/element/:uuid/scroll` | `direction`+`distance` \| `name` \| `predicateString` \| `toVisible` | scroll a container until a child is visible — a *semantic* scroll |
| `POST /wda/element/:uuid/scrollTo` | — | scroll this element into view |
| `POST /wda/element/:uuid/dragfromtoforduration` | `fromX/Y`,`toX/Y`,`duration` | element-relative drag |
| `POST /wda/element/:uuid/pressAndDragWithVelocity` | `pressDuration`,`toElement`,`velocity`,`holdDuration` | long-press-drag onto another element (reorder!) |
| `POST /wda/pickerwheel/:uuid/select` | `order` (`next`/`previous`), `offset?`, `value?`, `maxAttempts?` | **one-notch increment/decrement of a picker wheel** |
| `POST /element/:uuid/value` (`handleSetValue`) | `value`/`text` | type into text inputs; **PickerWheel → `adjustToPickerWheelValue:`; Slider → `adjustToNormalizedSliderPosition:` with a required 0..1 value** |
| `POST /element/:uuid/click`, `/clear` | | already used |

Coordinate twins exist for most (`/wda/touchAndHold`, `/wda/doubleTap`,
`/wda/pinch`, …) with optional `x`/`y`; without a session element they act on the
active application's center. The repo's W3C `/actions` path already covers the
coordinate cases it needs.

Notable non-element extras: `POST /wda/pressButton` supports `home`, `volumeUp`,
`volumeDown` (real devices), and `POST /wda/performIoHidEvent` takes an arbitrary
HID `page`/`usage`/`duration` — a future "named hardware key" surface.

Two accidental behaviors of today's code worth recording:

1. `set_value` (which calls `type_into` → `POST element/:id/value`) **already**
   adjusts a Slider if pointed at one — WDA interprets the string as a normalized
   0..1 position — and adjusts a PickerWheel via `adjustToPickerWheelValue`. This is
   undocumented, unvalidated (`set_value` never checks `kind`), and the reason a
   formal `perform` should own the non-text cases.
2. `increment`/`decrement` has **no single WDA primitive** except for picker wheels.
   For a Stepper, the tree exposes its two child `Button`s (labels usually
   "Increment"/"Decrement", localized); for a Slider, increment = read
   `value`/`min`/`max`, compute, set. For a bare `Adjustable`-trait `Other` element
   there is *no* reachable increment path (see §4).

### 1.5 What does not exist at all in WDA v9.15.3

- **`UIAccessibilityCustomAction`**: zero occurrences of `CustomAction` anywhere in
  `WebDriverAgentLib`. No route lists them, no snapshot key carries them.
- No route invokes `accessibilityIncrement`/`accessibilityDecrement` directly (XCUITest
  only exposes them indirectly through `adjustToPickerWheelValue`/`adjustToNormalizedSliderPosition`).
- No per-element "supported actions" query — iOS/XCUITest simply has no analogue of
  macOS AX's `AXActionNames` or Windows UIA's control patterns. Affordances must be
  **derived** from `type` + `traits` + `min/max`, which is precisely the design below.

## 2. Design

### 2.1 New sparse `ElementRow` fields (additive, `skip_serializing_if` like the rest)

```rust
/// Derived affordances beyond the universal tap/longpress, from type+traits+min/max.
/// Emitted only when non-empty; plain Buttons/Cells/StaticText emit nothing.
#[serde(skip_serializing_if = "Option::is_none")]
pub actions: Option<Vec<String>>,           // e.g. ["increment","decrement"]
/// From the `Selected` accessibility trait; only `true` is emitted.
#[serde(skip_serializing_if = "Option::is_none")]
pub selected: Option<bool>,
/// Slider/Stepper range, parsed from WDA's minValue/maxValue strings.
#[serde(skip_serializing_if = "Option::is_none")]
pub min: Option<f64>,
#[serde(skip_serializing_if = "Option::is_none")]
pub max: Option<f64>,
```

Derivation rules (daemon-side, in `flatten_tree`):

| condition | emitted `actions` |
|---|---|
| `kind == "PickerWheel"` | `["increment","decrement","adjust"]` |
| `kind == "Stepper"` (or both min/max present on non-slider) | `["increment","decrement"]` |
| `kind == "Slider"` | `["increment","decrement","adjust"]` |
| trait `Adjustable` on any other kind | `["increment","decrement"]` *(advisory — see §4 caveat)* |
| `kind == "Switch"` or trait `ToggleButton` | `["toggle"]` |
| trait `Selected` present | `selected: true` (state, not an action) |

Deliberately **not** emitted: `tap`, `longpress`/`menu`, `double_tap`, swipe/scroll —
universal to every hittable element, so listing them per row is pure payload bloat and
regresses the lean-JSON contract. `perform` still accepts them (§2.2); `actions` is the
*non-default* affordance list, not an exhaustive one.

Raw traits: do **not** emit a `traits` array by default (most values duplicate `kind`).
For debugging/forward-compat, gate a verbatim `"traits":[…]` field behind
`PHONE_REMOTE_ELEMENTS_TRAITS=1`, mirroring the `snapshot_settings_from_env` opt-in
pattern already in `wda.rs`.

No-regression analysis against the snapshot-token model:

- `element_snapshot_id` hashes the serialized rows (`http.rs:2906`), so new sparse
  fields change tokens only across daemon versions, never within a session —
  the token is already documented as ephemeral and cache capacity is 8.
- `diff_element_rows` identity is `(kind, label, identifier, placeholder)`
  (`http.rs:2976`) — untouched. New fields land on the *changed*-detection side,
  which is desirable: a tab flipping to `selected:true` now correctly shows as
  `changed` instead of `unchanged`.
- Trait stability: traits and min/max are static per control in practice
  (`UpdatesFrequently` etc. are bits, not live values), so tokens do not churn faster
  than today. `value` already changes more often than any new field will.
- All fields default to `None` → byte-identical JSON for every element that has no
  affordance, matching the `enabled`/`visible` sparse precedent exactly.

### 2.2 Generic invoke: `{"type":"perform", …}`

Request (both `POST /agent/input` control messages and `/agent/actions` steps):

```json
{"type":"perform","element":12,"snapshot":"aX3…","action":"increment"}
{"type":"perform","element":12,"snapshot":"aX3…","action":"menu","duration_ms":1200}
{"type":"perform","element":12,"snapshot":"aX3…","action":"adjust","value":"0.7"}
```

Action vocabulary v1 (allowlist, fail-closed):

| `action` | WDA dispatch (after the existing `fetch_snapshot_row` → `resolve_snapshot_row_element` pipeline) | extra params |
|---|---|---|
| `increment` / `decrement` | PickerWheel → `/wda/pickerwheel/:uuid/select` `{order:"next"/"previous",offset:0.2}`; Stepper → click the child Increment/Decrement `Button` (resolve via class-chain under the stepper); Slider → read `value,min,max`, step 10% of range, `POST element/:id/value` | — |
| `adjust` | PickerWheel → `element/:id/value` (= `adjustToPickerWheelValue`, today's `picker` path); Slider → `element/:id/value` with normalized 0..1 | `value` (string, required) |
| `menu` | `/wda/element/:uuid/touchAndHold` `{duration: duration_ms/1000, clamped 0.3–2.0s}` — the `perform_secondary_action` analogue: iOS's secondary-action surface *is* the long-press context menu | `duration_ms?` |
| `double_tap` | `/wda/element/:uuid/doubleTap` | — |
| `two_finger_tap` | `/wda/element/:uuid/twoFingerTap` | — |
| `scroll_to_visible` | `/wda/element/:uuid/scrollTo` | — |
| `toggle` | `element/:id/click` after asserting kind Switch / trait ToggleButton (semantic alias so agents need not special-case) | — |

Held back from v1 (add when a real task needs them): `pinch`, `rotate`, `force_press`,
`press_and_drag` (needs a second element target — new request shape), n-tap.

Response grammar: unchanged from every other snapshot-bound action — `{"ok":true}` on
success; failures reuse the existing fail-closed taxonomy verbatim
(`invalid_element_snapshot`, `stale_element_snapshot`, `element_not_found`,
`ambiguous_element_label`, `invalid_element_target`, `not_sent`/`outcome_unknown`
with `retry_safe`). Two additions:

- `unsupported_perform_action` — `action` not in the allowlist. `outcome:"not_sent"`,
  `retry_safe:false` (mirrors `unsupported_control`: retrying identical input cannot
  succeed).
- Element cannot carry the action (e.g. `increment` on a plain Button, `adjust` on a
  Cell): **reuse `invalid_element_target`** rather than minting a new code — its
  meaning is already "the matched element cannot carry this action", and its hint
  field can name the missing affordance. Keeps the taxonomy closed.

`SnapshotElementTapError` needs no new variants; the WDA-404 → `NotFound` remap
(`wda_error_is_missing_element`) applies to the new element routes identically.
`/wda/pickerwheel/:uuid/select` failures where the wheel refuses to move surface as
`AfterDispatch` (dispatched, outcome honest) — same contract as today's `picker`.

Batch validation (`validate_agent_action_value`), matching house style:

- `perform` requires `element` (u64) + `snapshot` (1–200 chars) + `action` (allowlist
  string); `duration_ms` optional u64 ≤ 10 000; `value` required iff `action=="adjust"`,
  ≤ 500 chars, forbidden otherwise; any unknown extra key for the given action →
  `invalid_actions_request` with a `steps[i]` detail.
- No `perform` action is uninstall-grade; no destructive-action carve-out needed
  (`menu` can reach Delete items, but so can `tap` — no new trust boundary).

### 2.3 Relationship to existing actions — alias or keep?

**Keep everything; nothing breaks; `perform` owns only what has no home today.**

| existing | verdict |
|---|---|
| `picker` (`adjustToPickerWheelValue` by column) | **Keep.** It is column-addressed (no snapshot needed) and hardware-validated (issue #23). `perform:"adjust"` on a PickerWheel row becomes the snapshot-bound sibling; document `picker` as the label-free/column form. Internally both bottom out in the same WDA call — share the client method, don't alias the wire format. |
| `set_value` | **Keep, and tighten.** Formally scope it to text-bearing kinds (TextField/SecureTextField/SearchField/TextView) at validation time; route Slider/PickerWheel writes to `perform:"adjust"`. Today's accidental slider-adjust via `set_value` becomes explicit instead of surprising. (Compat: nobody documented the accident; tightening is safe.) |
| `longpress` (coordinate) | **Keep.** It is the coordinate-mode primitive and the browser client sends it. `perform:"menu"` is the element-scoped semantic twin — same relationship `tap {x,y}` already has to `tap {element}`. Do not alias. |
| element `scroll` (`{element,dx,dy}` via W3C actions inside the rect) | **Keep as-is** — it is delta-based and validated. WDA's `/wda/element/:uuid/scroll {toVisible/predicateString}` is a *different*, stronger affordance ("scroll container until child X is visible"); if adopted later it should be a new `scroll_until` step, not a change to `scroll`. `perform:"scroll_to_visible"` covers the self-scoped case cheaply now. |
| `tap {element}` | Unchanged; `perform` deliberately has no `"tap"` so there is exactly one way to tap. |

This keeps `perform` = "invoke a *named affordance* on a snapshot-bound element",
while geometry stays with the existing gesture verbs. One new action type, zero
re-shaped old ones.

## 3. Honest limits (what LUMOS-grade affordance data WDA cannot give us)

1. **Custom actions are unreachable.** `UIAccessibilityCustomAction` lists (the
   rotor/"Actions available" items VoiceOver announces — e.g. Mail's per-row
   Archive/Flag) cross neither XCUITest's public API nor WDA v9.15.3. They live as
   name+target+selector objects inside the app process; the AX serialization that
   testmanagerd hands XCUITest does not carry them.
2. **Fork scope, if ever wanted:** the repo pins WDA by tag, so a small carried patch
   is thinkable. WDA already injects custom snapshot request parameters
   (`Categories/XCAXClient_iOS+FBSnapshotReqParams.m`) — the patch would add the AX
   custom-actions attribute to the requested set and a
   `POST /wda/element/:uuid/performAccessibilityAction` route calling private
   `XCAXClient` machinery. Estimated 2–4 days of fork spike work with a **genuinely
   uncertain outcome** (it is unknown whether testmanagerd serializes the attribute
   across the process boundary at all; names might come through, invocation might
   not). Recommendation: do not schedule until the trait-based tier proves
   insufficient on a real task.
3. **`Adjustable` without a typed control is advisory only.** For an `Other` row with
   the `Adjustable` trait there is no WDA increment path (no pickerwheel route, no
   min/max, no child buttons guaranteed). Emitting `["increment","decrement"]` for it
   tells the agent the affordance *exists*; `perform` on it must return
   `invalid_element_target` until a fork adds real AX increment. Option: mark these
   `actions:["increment?","decrement?"]`… — rejected, sparse strings should stay
   machine-exact; instead only emit derived actions for kinds we can actually drive
   (PickerWheel/Stepper/Slider/Switch), and surface bare `Adjustable` via the
   env-gated raw `traits` field until driveable.
4. **`hittable` stays out of the tree** at this WDA version (attribute-only, N+1
   round-trips); `ToggleButton`/`SupportsZoom` traits require the runner to be built
   with a modern Xcode (true for this repo's setup scripts).
5. Trait fidelity varies by framework: SwiftUI sometimes materializes different
   trait/type combinations than UIKit for the "same" control; the derivation table
   keys on both `kind` and traits for that reason.

## 4. Feasibility verdict

**Feasible, and cheaper than expected — the core requires no WDA change at all.**
`traits`, `minValue`, and `maxValue` are already present in every `/source?format=json`
response the daemon fetches; surfacing affordances is a parse-and-derive change in
`flatten_tree` plus one new snapshot-bound action type that reuses the entire existing
`fetch_snapshot_row`/`resolve_snapshot_row_element`/error-taxonomy pipeline and
WDA's existing element-scoped routes. The only out-of-reach piece is
`UIAccessibilityCustomAction` (WDA has zero support; a carried fork patch is a 2–4 day
spike with uncertain yield) — everything else in the LUMOS pattern maps onto data WDA
already emits.

### Next-step spike (recommended)

Behind `PHONE_REMOTE_ELEMENTS_AFFORDANCES=1` (default off ⇒ byte-identical JSON):

1. Parse `traits` / `minValue` / `maxValue` in `flatten_tree`; emit sparse
   `selected`, `min`, `max`, and derived `actions` per §2.1, with unit tests mirroring
   `flatten_captures_value_for_switch_slider_picker`.
2. Hardware-validate on Settings (Wi-Fi switch → `toggle`; Brightness slider →
   `min/max/actions`), a date picker (`PickerWheel` → `increment`), and one tab bar
   (`selected:true`), confirming token/delta behavior is unchanged with the flag off.

Estimated effort: **0.5–1 day** including tests (no daemon-protocol change, no web
client change). The `perform` action itself is the follow-up: ~1–2 days, starting with
`increment`/`decrement`/`menu` only, since those three unlock pickers, steppers, and
context menus — the concrete tasks (date pickers, quantity steppers, per-row menus)
that today force coordinate guessing.
