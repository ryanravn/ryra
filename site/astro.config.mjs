// @ts-check
import { defineConfig } from "astro/config";
import starlight from "@astrojs/starlight";
// https://astro.build/config
export default defineConfig({
  site: "https://docs.ryra.dev",
  integrations: [
    starlight({
      title: "Ryra",
      customCss: ["./src/styles/custom.css"],
      expressiveCode: {
        themes: ["github-dark"],
        styleOverrides: {
          borderRadius: "12px",
          borderColor: "color-mix(in oklab, #0e2230 40%, #1a1410)",
          codeBackground: "#0e2230",
          codeFontFamily:
            '"JetBrains Mono", ui-monospace, SFMono-Regular, Menlo, Consolas, monospace',
          frames: {
            shadowColor: "rgba(14, 34, 48, 0.55)",
            frameBoxShadowCssValue:
              "0 1px 0 rgba(255, 255, 255, 0.05) inset, 0 30px 60px -20px rgba(14, 34, 48, 0.55), 0 8px 20px -8px rgba(14, 34, 48, 0.4)",
            terminalBackground: "#0e2230",
            terminalTitlebarBackground:
              "linear-gradient(180deg, #1a3a52 0%, #122c3e 100%)",
            terminalTitlebarBorderBottomColor: "#00000040",
            terminalTitlebarDotsForeground: "#8aa9bf",
            editorBackground: "#0e2230",
            editorTabBarBackground: "#08111a",
            editorActiveTabBackground: "#0e2230",
            editorActiveTabIndicatorTopColor: "#d97a3a",
          },
        },
      },
      head: [
        {
          tag: "link",
          attrs: { rel: "preconnect", href: "https://fonts.googleapis.com" },
        },
        {
          tag: "link",
          attrs: { rel: "preconnect", href: "https://fonts.gstatic.com", crossorigin: "" },
        },
        {
          tag: "link",
          attrs: {
            rel: "stylesheet",
            href: "https://fonts.googleapis.com/css2?family=Fraunces:opsz,wght@9..144,400;9..144,500;9..144,600;9..144,700&family=Inter:wght@400;500;600;700&family=JetBrains+Mono:wght@400;500;600&display=swap",
          },
        },
      ],
      social: [
        {
          icon: "github",
          label: "GitHub",
          href: "https://github.com/ryanravn/ryra",
        },
      ],
      sidebar: [
        { label: "Introduction", slug: "intro" },
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
