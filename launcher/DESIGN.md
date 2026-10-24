# Open Harness UI — design system notes

Single-file app UI (`ui/index.html`), no build step, no runtime deps. This note keeps
future passes consistent.

## Concept — "quiet cockpit"

A local engine's instrument panel: calm dark ground, one blue accent, 1px lines,
everything answers instantly. It is a work tool; spectacle stays out, craft stays in.

- **Metaphor:** cockpit instrumentation — status that is glanceable, controls that respond
  the moment they are touched.
- **Motion verb:** *settle* — damped, quick (150–260 ms), `cubic-bezier(.23,1,.32,1)`.
  Elements arrive from 4–8 px away, never bounce, never slide across the screen.
- **Signature moment:** the composer *waking* on focus (accent border + soft glow ring)
  and the empty state as the poster frame (logo → title → suggestion chips, one-time
  staggered settle).
- **Rest state:** intentionally still. Life at rest lives only in real state: the busy
  status pulse, the streaming cursor. No decorative wobble (decision, per awwwards
  reduced-motion/idle rules).

## Tokens

- Palette: near-black neutrals in the ZCode manner — `--bg #0a0a0d`, `--side #0d0d10`,
  `--main #111114`, `--card #17171b`, `--acc #4f8cff`; blue for progress/links, **amber
  `--mode #e8a33d` for the Tools mode pill**, green/red carry status only, never decoration.
- Motion: `--ease` (settle), `--ease-out` (arrivals), `--glow` focus ring
  (`0 0 0 3px rgba(79,140,255,.13)`).
- Type: system Segoe UI stack (desktop app — no webfont), mono for numbers/paths/ids.
- Radius rule: 16 outer / 12 cards / 8 controls / 999 pills.

## Signature surfaces (ZCode-inspired)

- Sidebar: ghost "New chat" with a `Ctrl+N` kbd chip (shortcut wired), quiet secondary
  actions, searchable chat list, mono timestamps.
- Empty state: two-column action-card grid (Agent workspace / Attach files / Skills /
  Models) with accent icon tiles + one-time staggered settle; prompt chips below.
- Composer: "+" attach on the left, amber Tools mode pill, ctx gauge that opens a
  **context popover** (used/limit %, per-part breakdown rows with colored dots, model line).
- Attachments render as cards (extension tag in an icon tile + name + kind label), in the
  composer and inside sent messages.

## Behavior rules

- Every interactive element: hover + `:focus-visible` + active (press = `scale(.985)`,
  primary CTAs also lift 1 px). One treatment per role, reused.
- Chat messages are **not** animated: `renderChat()` re-renders every node on each
  stream token, so entrance animations would replay constantly. Motion belongs to
  surfaces that mount once (menus, dialogs, toast, empty state, attachment bar changes).
- `prefers-reduced-motion`: global kill-switch for transitions/animations; logo keeps its
  explicit end-state block.
- Accessibility: icon-only buttons carry `aria-label`, chat log is `role="log"`,
  toast is `role="status"`, visible focus ring everywhere, AA contrast on dark ground.
