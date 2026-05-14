import type { APIRoute } from "astro";
import { getCollection } from "astro:content";

const SITE = "https://ryra.dev";
const REPO = "https://github.com/ryanravn/ryra";

// intro first, then alphabetical by slug.
const sortDocs = (a: { id: string }, b: { id: string }) => {
  if (a.id === "intro") return -1;
  if (b.id === "intro") return 1;
  return a.id.localeCompare(b.id);
};

export const GET: APIRoute = async () => {
  const docs = (await getCollection("docs")).sort(sortDocs);

  const lines: string[] = [
    "# Ryra",
    "",
    "> CLI that deploys self-hosted services on a single Linux machine using rootless Podman and systemd quadlets. Run `ryra add <service>` and Ryra scaffolds the container, reverse proxy route, SSO client, and systemd unit from a curated registry.",
    "",
    "Designed for a single Linux host. Services run under the invoking user's rootless Podman; ryra-core is a pure library with no sudo or side effects, ryra-cli is a thin shell that applies changes. Optional integrations: Caddy for HTTPS, Authelia for OIDC SSO, SMTP for outbound mail.",
    "",
    "## Docs",
    "",
  ];

  for (const entry of docs) {
    const url = `${SITE}/${entry.id}/`;
    const desc = entry.data.description ? `: ${entry.data.description}` : "";
    lines.push(`- [${entry.data.title}](${url})${desc}`);
  }

  lines.push(
    "",
    "## Services",
    "",
    `- [Service registry](${SITE}/services/): Catalog of services Ryra can deploy (Immich, Nextcloud, Forgejo, Vaultwarden, Authelia, and more), grouped by category.`,
    "",
    "## Source",
    "",
    `- [GitHub repository](${REPO}): Rust source, registry definitions, and issue tracker.`,
    `- [Install script](${REPO}/raw/main/install.sh): \`curl -fsSL ${REPO}/raw/main/install.sh | sh\` detects the distro and sets up the package repo.`,
    "",
    "## Optional",
    "",
    `- [Full docs as one file](${SITE}/llms-full.txt): The entire docs corpus concatenated as plain markdown, for ingestion in a single fetch.`,
    `- [Rust API docs](https://docs.rs/ryra): docs.rs reference for ryra-core, ryra-cli, ryra-vm, and ryra-test.`,
  );

  return new Response(lines.join("\n") + "\n", {
    headers: { "Content-Type": "text/plain; charset=utf-8" },
  });
};
