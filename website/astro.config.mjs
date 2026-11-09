import { defineConfig } from 'astro/config';
import sitemap from '@astrojs/sitemap';

// Deployment knobs. Defaults match a GitHub Pages project site; override with
//   SITE_URL=https://pi.dev BASE_PATH=/ npm run build
const site = process.env.SITE_URL ?? 'https://neonity2020.github.io';
const base = process.env.BASE_PATH ?? '/pi-os';

// https://astro.build/config
export default defineConfig({
	site,
	base,
	trailingSlash: 'never',
	integrations: [
		sitemap({
			// Astro maps the 404 route onto the site root, so keep only the
			// canonical landing-page URL in the sitemap.
			filter: (page) => page.endsWith('/'),
		}),
	],
	build: {
		format: 'directory',
	},
});
