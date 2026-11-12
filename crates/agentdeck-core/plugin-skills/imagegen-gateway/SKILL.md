---
name: imagegen-gateway
description: "Last-resort raster image generation and editing through a paid API gateway (agentdeck imagegen: Vercel AI Gateway or OpenRouter). Use ONLY when it is the only working way to produce an image in this harness: no built-in image tool (such as Codex image_gen) and no other image-generation skill is available or they failed. Covers photos, illustrations, textures, sprites, product shots, UI mockups, backgrounds, and edits of an existing image. Not for charts, diagrams, or icons/logos better built as SVG/HTML/CSS."
---

# Image generation through the gateway

Use this skill only when nothing else in this harness can make images: a built-in image tool
or another image skill always wins. Every call is billed to the owner's gateway account
(Seedream 5.0 Pro: about $0.035 per image), so generate what the task needs, not extra variants.

```bash
agentdeck imagegen -o OUT.png [-a 16:9] [-e EDIT_TARGET] [-r REF]... "PROMPT"   # '-' reads stdin
```

- One call = one image, 30-120 s; run it with a timeout of at least 300 s. It prints one JSON
  line `{"path","width","height","model","seconds","cost"}`, or an error and exit 1.
- `-o` never overwrites (exit 2) unless `--force`. The extension follows the returned format,
  so the printed `path` may end in `.jpg` even if you asked for `.png`.
- `-a`: `1:1` (default) `16:9 9:16 4:3 3:4 3:2 2:3 21:9`; output is about 4 MP.
- `-e FILE`: the image to edit (Image 1); its shape is kept unless `-a` is given.
  `-r FILE`: style/composition/subject reference, repeatable (Image 2...).
- `--model provider/model` overrides the configured model for one call; `--provider
  vercel|openrouter` switches gateway. Defaults come from `[imagegen]` in agentdeck's config.
- No transparency: ask for a plain flat background and cut it out afterwards if needed.

## Workflow

1. Intent: **edit** (change an existing image, keep the rest) or **generate** (no image, or images
   only as references). Default to generate.
2. Destination: the path the user named, else the project location that will use the asset.
   Never leave a deliverable in `/tmp`. For an existing asset write a sibling (`hero-v2.png`)
   unless the user asked to replace it.
3. Write the prompt with the spec below, then run the command.
4. Look at every result (a resized copy if large) and check subject, style, composition, exact
   text and the avoid list. A call can succeed and still miss the brief.
5. Iterate with one targeted change at a time; for edits restate what must stay unchanged.
6. Report the final path(s), the final prompt and the cost line. Update code that uses the asset.

Errors: `401/403` = key or plan problem, `404` with "guardrail" = the OpenRouter workspace blocks
that provider, `429` = rate limit (retry once later). Report them; do not loop or switch to a
pricier model without asking.

## Prompt spec

Use only the lines that help; keep it short. A specific prompt gets normalized, not embellished;
a generic one may get composition, lighting or intended-use detail. Never add characters,
brands, slogans or palettes the request does not imply.

```text
Use case: <photorealistic-natural | product-mockup | ui-mockup | infographic-diagram | ads-marketing | logo-brand | illustration-story | stylized-concept | game-asset | precise-object-edit | style-transfer | lighting-weather | compositing>
Asset type: <where it will be used>
Primary request: <the main ask>
Scene/backdrop: <environment>
Subject: <main subject>
Style/medium: <photo / flat illustration / 3D render / pixel art...>
Composition/framing: <wide / close-up / top-down; placement; negative space for copy>
Lighting/mood: <...>
Color palette: <...>
Text (verbatim): "<exact text>"   (spell tricky words letter by letter)
Constraints: <change only X; keep Y unchanged>
Avoid: <no watermark, no logos, no extra text...>
```

Tips: scene, then subject, details, constraints; camera and lens language for photoreal; state
the intended use; UI mockups show the interface only, without a device frame; refer to inputs
by index ("keep the pose from Image 2").
