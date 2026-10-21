# Backend identity assets

Retrieved 2026-10-20. `sources.json` records exact URLs, immutable upstream
revisions, archive members and SHA-256 hashes. Asset files are byte-for-byte
upstream originals (including Muse, retained at `../muse.svg`). SVG geometry,
view boxes, clear space and colors are not edited. No icon packs, tracing or
AI-generated artwork are used. OpenAI's supplied Blossom identifies the Codex
provider; it is not claimed to be a distinct Codex product icon.

`backend_icon()` embeds these bytes as data URLs: no network access is needed.
SVG image containment preserves aspect ratio and isolates internal IDs. The
existing 24px slot/CSS hook remains available to callers, including 18px rows.
OpenAI and OpenCode select vendor-supplied light/dark colorways using Tyde's
`data-theme` color scheme. Tycode has a white backing for its dark artwork; Grok has a black backing
for its white artwork. Neither original glyph is recolored. Icons are decorative
(`aria-hidden`, non-focusable); callers must supply the backend's visible name.

## Rights and usage requirements — not a clearance certification

Public availability establishes provenance, **not permission**. The repository
license does not relicense third-party marks. No Tyde approval, endorsement,
partnership, or guarantee of trademark compliance is asserted. Copyright
licenses and trademark/brand requirements are distinct. Distribution owners
remain responsible for the applicable terms, approvals and updates below.

- **OpenAI / Codex:** [official brand kit and usage terms](https://openai.com/brand/).
  Use exactly as provided, only in relation to OpenAI services, with prescribed
  spacing; do not imply endorsement or make it primary branding. Permission is
  limited, non-transferable and revocable. Both black and white SVGs come from
  the official kit; its original clear space is retained.
- **Claude:** [official press kit](https://www.anthropic.com/press-kit), linked
  from [Anthropic's newsroom](https://www.anthropic.com/news), supplies the clay
  Spark. [Trademark guidelines](https://www.anthropic.com/legal/trademark-guidelines)
  require specific permission and advance approval of materials, supplied
  artwork, no alteration, readable backgrounds and reasonable spacing. This
  integration does not establish that Tyde has those approvals.
- **Kiro:** the official [homepage](https://kiro.dev/) links `icon.svg`.
  [AWS trademark license terms](https://aws.amazon.com/trademark-guidelines/)
  §§3–4 limit licenses and require written permission outside their scope and
  advance approval of materials. No separate redistribution license for the
  website icon was located; Tyde approval is not established.
- **Antigravity:** [official press assets](https://antigravity.google/press)
  supply the full-color icon. See Google's [brand guidance](https://about.google/brand-resource-center/guidance/),
  [product guidance](https://about.google/brand-resource-center/products-and-services/)
  and [brand terms](https://about.google/intl/en/brand-resource-center/brand-terms/).
  The press download does not itself establish permission for application
  redistribution; product-icon use may require approval. None is asserted here.
- **Hermes:** [Nous Research's upstream assets](https://github.com/NousResearch/hermes-agent/tree/0b8dee00fafe975f10be40172322d0ccfabfa7f6/assets).
  Upstream [MIT license](https://github.com/NousResearch/hermes-agent/blob/0b8dee00fafe975f10be40172322d0ccfabfa7f6/LICENSE)
  is retained in `hermes-LICENSE`; it is not a trademark approval.
- **Grok:** original PNG from [xAI's own cookbook example](https://github.com/xai-org/xai-cookbook/blob/58b8d68bfc1ef8fff0a9c0561342e6d11d4b9571/examples/voice-agent-web/client/public/grok.png).
  [Brand guidelines](https://x.ai/legal/brand-guidelines) require accurate
  reference, unaltered supplied logos, no implied endorsement, and reserve
  revocation rights. Their linked `https://data.x.ai/logos/SpaceXAI_Grok_Assets.zip`
  returned HTTP 403 on this host, including with a browser user agent/referrer.
  The official upstream PNG is used instead, not a third-party copy or the
  SpaceX/xAI corporate mark. Upstream's [Beta Testing License](https://github.com/xai-org/xai-cookbook/blob/58b8d68bfc1ef8fff0a9c0561342e6d11d4b9571/LICENSE)
  is retained in `grok-LICENSE`: it expressly permits distribution but also
  describes the software as intended solely for testing/evaluation. That
  limitation and the brand guideline's reference to its download kit remain
  rights limitations for production use; this is not a production clearance.
- **OpenCode:** original light/dark SVGs from the official
  [brand assets](https://opencode.ai/brand), mirrored in the pinned official
  repository. [MIT license](https://github.com/anomalyco/opencode/blob/5fa426f68d1dbf3c8aaa01557e357719f0577438/LICENSE)
  is retained in `opencode-LICENSE`; trademark ownership is unaffected.
- **Tycode:** original VS Code icon from the owner's
  [upstream repository](https://github.com/tigy32/Tycode/blob/4766e3935f51d940883222e40bac941d4e02a9ae/tycode-vscode/tycode-icon.svg).
  Its [MIT license](https://github.com/tigy32/Tycode/blob/4766e3935f51d940883222e40bac941d4e02a9ae/LICENSE)
  is retained in `tycode-LICENSE`. This replaces the generic code brackets
  rather than representing them as vendor artwork.
- **Muse:** existing [official SVG](https://muse.ai/images/favicon/app-squiggle.svg)
  was retrieved again and byte-compared unchanged. The current [service terms](https://muse.ai/terms)
  do not establish a separate asset redistribution license. No such license or
  trademark approval is claimed for the already bundled mark.
