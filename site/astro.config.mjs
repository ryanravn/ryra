// @ts-check
import { defineConfig } from "astro/config";
import starlight from "@astrojs/starlight";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const registry = require("../registry.json");

const serviceItems = Object.entries(registry.services).map(([id, entry]) => ({
  label: entry.service.name.charAt(0).toUpperCase() + entry.service.name.slice(1),
  link: `/services/${id}/`,
}));

// https://astro.build/config
export default defineConfig({
  site: "https://docs.ryra.dev",
  integrations: [
    starlight({
      title: "Ryra",
      social: [
        {
          icon: "github",
          label: "GitHub",
          href: "https://github.com/ryanravn/ryra",
        },
      ],
      sidebar: [
        { label: "Introduction", link: "/" },
        {
          label: "Getting Started",
          items: [
            { label: "Installation", slug: "getting-started/installation" },
            { label: "Quick Start", slug: "getting-started/quick-start" },
          ],
        },
        {
          label: "Services",
          items: [
            { label: "Overview", link: "/services/" },
            ...serviceItems,
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
          items: [
            {
              label: "Rust Docs",
              link: "https://docs.rs/ryra",
              attrs: { target: "_blank" },
            },
          ],
        },
      ],
    }),
  ],
});
