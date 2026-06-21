# CloudFront one-time setup for `/tyde/*`

This is a **one-time manual setup** the repo owner runs. It adds a dedicated
cache behavior for `tyde/*` to the existing `tycode.dev` distribution and
attaches a security `ResponseHeadersPolicy` (CSP + HSTS + CORS) that the web
loader needs.

> **The single most important rule:** the new `ResponseHeadersPolicy` is
> attached **ONLY** to the new `tyde/*` cache behavior — **never** to the
> default behavior. The default behavior serves the live marketing site
> (`index.html`, `blog.html`, `posts/…`, etc.); slapping the loader's strict CSP
> on those pages would break them. Recon confirmed the default behavior
> currently has **no** ResponseHeadersPolicy (`DefaultRHP: null`) — keep it that
> way.

Nothing here is run automatically. `deploy.sh` assumes this behavior already
exists; it only ever does `aws s3 sync` (additive) + `create-invalidation`.

## Ground truth (from read-only recon)

| Thing | Value |
| --- | --- |
| Account / region | `814147156407` / `us-west-2` |
| Distribution (tycode.dev) | `E3JJ1OF4I8TP6U` |
| S3 origin id | `tycode-static.s3.us-west-2.amazonaws.com-mfxkzqqpcb1` |
| S3 origin domain | `tycode-static.s3-website-us-west-2.amazonaws.com` (S3 **website** endpoint) |
| Default cache policy | `658327ea-f89d-4fab-a63d-7e88639e58f6` (Managed-CachingOptimized) |
| Default ResponseHeadersPolicy | **none** (must stay none) |
| Existing ordered cache behaviors | **0** |
| Viewer protocol | `redirect-to-https` |

`ETag` values shown below are **point-in-time** and change on every mutation —
always re-read the current `ETag` immediately before an `--if-match` update.

---

## Step 1 — Create the security ResponseHeadersPolicy

The CSP mirrors the loader's `index.html` `<meta>` CSP **plus** `frame-ancestors
'none'` (which `<meta>` cannot express). Load-bearing directives:
`script-src 'self' 'wasm-unsafe-eval'` (same-origin JS + WASM compile, **no**
general `unsafe-eval`), `connect-src 'self' wss:` (broker over wss + same-origin
fetch), `object-src 'none'`, `base-uri 'none'`, `frame-ancestors 'none'`. HSTS
is added here too (it cannot be set via `<meta>`). CORS is included so the
loader's `crossorigin="anonymous"` + SRI fetches always succeed.

Save this as `tyde-rhp.json`:

```json
{
  "Name": "tyde-loader-security-headers",
  "Comment": "CSP+HSTS+CORS for the Tyde web loader. Attach ONLY to the tyde/* behavior.",
  "SecurityHeadersConfig": {
    "ContentSecurityPolicy": {
      "Override": true,
      "ContentSecurityPolicy": "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; media-src 'self' blob:; connect-src 'self' wss:; worker-src 'self'; manifest-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'"
    },
    "StrictTransportSecurity": {
      "Override": true,
      "IncludeSubdomains": true,
      "Preload": true,
      "AccessControlMaxAgeSec": 63072000
    },
    "ContentTypeOptions": { "Override": true },
    "FrameOptions": { "Override": true, "FrameOption": "DENY" },
    "ReferrerPolicy": { "Override": true, "ReferrerPolicy": "no-referrer" }
  },
  "CorsConfig": {
    "AccessControlAllowOrigins": { "Quantity": 1, "Items": ["https://tycode.dev"] },
    "AccessControlAllowHeaders": { "Quantity": 1, "Items": ["*"] },
    "AccessControlAllowMethods": { "Quantity": 2, "Items": ["GET", "HEAD"] },
    "AccessControlAllowCredentials": false,
    "AccessControlExposeHeaders": { "Quantity": 0, "Items": [] },
    "AccessControlMaxAgeSec": 600,
    "OriginOverride": true
  }
}
```

Create it (note the policy id it returns — call it `RHP_ID`):

```sh
aws cloudfront create-response-headers-policy \
  --response-headers-policy-config file://tyde-rhp.json \
  --query 'ResponseHeadersPolicy.Id' --output text
# -> RHP_ID, e.g. 1a2b3c4d-....  (save this)
```

---

## Step 2 — Add the `tyde/*` cache behavior

CloudFront has no "add one behavior" API — you fetch the full distribution
config, splice in the ordered behavior, and `update-distribution` with the
current `ETag`. The recipe below uses `jq` so you don't hand-edit the whole
config.

```sh
DIST=E3JJ1OF4I8TP6U
ORIGIN_ID='tycode-static.s3.us-west-2.amazonaws.com-mfxkzqqpcb1'
CACHE_POLICY='658327ea-f89d-4fab-a63d-7e88639e58f6'   # Managed-CachingOptimized
RHP_ID='<paste RHP_ID from Step 1>'

# 1. Pull current config + ETag.
aws cloudfront get-distribution-config --id "$DIST" > dist.json
ETAG=$(jq -r '.ETag' dist.json)

# 2. Build the new ordered behavior and splice it in (PathPattern has NO leading slash).
jq --arg origin "$ORIGIN_ID" --arg cp "$CACHE_POLICY" --arg rhp "$RHP_ID" '
  .DistributionConfig as $cfg
  | $cfg
  | .CacheBehaviors.Items = (($cfg.CacheBehaviors.Items // []) + [{
      "PathPattern": "tyde/*",
      "TargetOriginId": $origin,
      "ViewerProtocolPolicy": "redirect-to-https",
      "CachePolicyId": $cp,
      "ResponseHeadersPolicyId": $rhp,
      "Compress": true,
      "AllowedMethods": {
        "Quantity": 2, "Items": ["GET","HEAD"],
        "CachedMethods": { "Quantity": 2, "Items": ["GET","HEAD"] }
      },
      "SmoothStreaming": false,
      "FieldLevelEncryptionId": "",
      "LambdaFunctionAssociations": { "Quantity": 0 },
      "FunctionAssociations": { "Quantity": 0 },
      "TrustedSigners": { "Enabled": false, "Quantity": 0 },
      "TrustedKeyGroups": { "Enabled": false, "Quantity": 0 }
    }])
  | .CacheBehaviors.Quantity = (.CacheBehaviors.Items | length)
' dist.json > new-config.json

# 3. Apply. The default behavior is untouched — verify before/after that it has
#    NO ResponseHeadersPolicyId.
aws cloudfront update-distribution \
  --id "$DIST" \
  --if-match "$ETAG" \
  --distribution-config file://new-config.json
```

> If `jq` isn't available, edit `dist.json` by hand: take the object under
> `.DistributionConfig`, append the behavior above to
> `.CacheBehaviors.Items`, bump `.CacheBehaviors.Quantity`, and pass just the
> `DistributionConfig` object (not the wrapping `{ETag, DistributionConfig}`) to
> `update-distribution`.

---

## Step 3 — Verify (read-only)

```sh
# The tyde/* behavior exists and carries the RHP:
aws cloudfront get-distribution-config --id E3JJ1OF4I8TP6U \
  --query "DistributionConfig.CacheBehaviors.Items[?PathPattern=='tyde/*'].{Path:PathPattern,Origin:TargetOriginId,RHP:ResponseHeadersPolicyId}"

# The DEFAULT behavior still has NO RHP (marketing pages stay un-CSP'd):
aws cloudfront get-distribution-config --id E3JJ1OF4I8TP6U \
  --query 'DistributionConfig.DefaultCacheBehavior.ResponseHeadersPolicyId'
# -> must print: null

# After deploy + propagation, headers on a tyde asset:
curl -sI https://tycode.dev/tyde/ | grep -iE 'content-security-policy|strict-transport-security|x-content-type-options'
# A marketing page must NOT carry the loader CSP:
curl -sI https://tycode.dev/index.html | grep -i 'content-security-policy' || echo 'no CSP on marketing (correct)'
```

## Notes / gotchas

- **S3 website origin, not REST.** The origin is the S3 *website* endpoint
  (`…s3-website-…`), so it speaks plain HTTP and serves `index.html` for
  directory requests like `/tyde/`. Don't switch it to a REST/OAC origin as part
  of this change.
- **CSP via header AND meta.** The loader keeps its `<meta>` CSP for local/no-CDN
  use; the header from this RHP is the authoritative one in production and adds
  `frame-ancestors` + HSTS. Keep the two in sync if you edit either — the header
  string above is the `<meta>` policy plus `frame-ancestors 'none'`.
- **`connect-src 'self' wss:`** intentionally omits `https:`. If a future feature
  needs cross-origin HTTPS, add it in both this RHP and `web/loader/index.html`.
- **Don't touch** the `tyggs.*` assets, the marketing keys at the bucket root, or
  the default behavior. This change is purely additive: one new origin-less
  behavior + one new RHP.
