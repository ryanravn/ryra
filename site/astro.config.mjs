// @ts-check
import { defineConfig } from "astro/config";
import starlight from "@astrojs/starlight";
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
