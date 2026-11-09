import type { APIRoute } from 'astro';

export const GET: APIRoute = ({ site }) => {
	const base = import.meta.env.BASE_URL.endsWith('/')
		? import.meta.env.BASE_URL
		: `${import.meta.env.BASE_URL}/`;
	const origin = site ?? new URL('http://localhost:4321');
	const sitemap = new URL(`${base}sitemap-index.xml`, origin).href;

	return new Response(
		['User-agent: *', 'Allow: /', '', `Sitemap: ${sitemap}`, ''].join('\n'),
		{ headers: { 'Content-Type': 'text/plain; charset=utf-8' } },
	);
};
