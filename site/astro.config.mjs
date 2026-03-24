// @ts-check
import { defineConfig } from "astro/config";
import starlight from "@astrojs/starlight";

// https://astro.build/config
export default defineConfig({
  site: "https://ryra.dev",
  integrations: [
    starlight({
      title: "Ryra",
      customCss: ["./src/styles/custom.css"],
      social: [
        {
          icon: "github",
          label: "GitHub",
          href: "https://github.com/ryanravn/ryra",
        },
      ],
      sidebar: [
        {
          label: "Getting Started",
          items: [
            { label: "Introduction", slug: "getting-started/introduction" },
            { label: "Installation", slug: "getting-started/installation" },
            { label: "Quick Start", slug: "getting-started/quick-start" },
          ],
        },
        {
          label: "Services",
          items: [
            { label: "Overview", slug: "services/overview" },
            { label: "Vaultwarden", slug: "services/vaultwarden" },
            { label: "Forgejo", slug: "services/forgejo" },
            { label: "Uptime Kuma", slug: "services/uptime-kuma" },
            { label: "OpenClaw", slug: "services/openclaw" },
            { label: "PostgreSQL", slug: "services/postgres" },
          ],
        },
        {
          label: "Guides",
          items: [
            { label: "Configuration", slug: "guides/configuration" },
            { label: "Exposure Modes", slug: "guides/exposure-modes" },
          ],
        },
        {
          label: "Reference",
          items: [{ label: "CLI Commands", slug: "reference/cli" }],
        },
      ],
    }),
  ],
});
