# pi-rs website

Marketing landing page for [pi-rs](https://github.com/Neonity2020/pi-os), built with
[Astro](https://astro.build). Static output, no client framework, no runtime dependency
on Node — the deployed artifact is plain HTML, CSS and a few kilobytes of JavaScript.

## Commands

```bash
npm install
npm run dev       # http://localhost:4321/pi-os/  (base path is /pi-os)
npm run check     # astro check — TypeScript/template diagnostics
npm run build     # static output into dist/
npm run preview   # serve dist/ locally
```

## Layout

```
src/
  layouts/BaseLayout.astro   # <head>, meta/OG tags, scroll-reveal script
  components/                # one section per component, scoped styles
  pages/index.astro          # the landing page (composition only)
  pages/404.astro            # not indexed, no canonical
  pages/robots.txt.ts        # generated at build time, points at the sitemap
  styles/global.css          # design tokens + shared utilities
public/                      # favicon.svg, og.svg, og.png (static copies)
```

## Deployment

The site is deployed to GitHub Pages as a project page:
`https://neonity2020.github.io/pi-os/`.

`.github/workflows/website.yml` builds and publishes on every push to `main` that
touches `website/`. The build reads two environment variables:

| Variable    | Default                        | Meaning                        |
| ----------- | ------------------------------ | ------------------------------ |
| `SITE_URL`  | `https://neonity2020.github.io` | origin used for canonical/OG   |
| `BASE_PATH` | `/pi-os`                       | path the site is served under  |

To publish the same build under a different origin, override them:

```bash
SITE_URL=https://pi.dev BASE_PATH=/ npm run build
```

Keep `BASE_PATH` in sync with `SITE_URL` — `SITE_URL` is the origin only, the path
belongs to `BASE_PATH`. The sitemap, `robots.txt`, canonical URLs and OG image URLs
are all derived from these two values.

## Assets

- `public/favicon.svg` — mark used by the browser tab.
- `public/og.svg` — source of the social card.
- `public/og.png` — 1200×630 raster used by `og:image` (social crawlers do not read SVG).

Regenerate the PNG after editing the SVG:

```bash
node -e "require('sharp')('public/og.svg',{density:144}).resize(1200,630).png().toFile('public/og.png')"
```
