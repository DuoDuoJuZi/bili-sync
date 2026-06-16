import { sveltekit } from '@sveltejs/kit/vite';
import tailwindcss from '@tailwindcss/vite';
import { defineConfig } from 'vite';

export default defineConfig({
	plugins: [tailwindcss(), sveltekit()],
	server: {
		proxy: {
			'/api/ws': {
				target: 'ws://localhost:49231',
				ws: true,
				rewriteWsOrigin: true
			},
			'/api': 'http://localhost:49231'
		},
		host: true
	}
});
