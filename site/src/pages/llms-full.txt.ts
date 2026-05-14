import type { APIRoute } from "astro";
import { getCollection } from "astro:content";

// intro first, then alphabetical by slug.
const sortDocs = (a: { id: string }, b: { id: string }) => {
  if (a.id === "intro") return -1;
  if (b.id === "intro") return 1;
  return a.id.localeCompare(b.id);
};

export const GET: APIRoute = async () => {
  const docs = (await getCollection("docs")).sort(sortDocs);

  const parts: string[] = [
    "# Ryra",
    "",
    "> CLI that deploys self-hosted services on a single Linux machine using rootless Podman and systemd quadlets.",
    "",
  ];

  for (const entry of docs) {
    const body = (entry.body ?? "")
      .replace(/^import\s+.*?from\s+["'][^"']+["'];?\s*$/gm, "")
      .trim();
    parts.push(`# ${entry.data.title}`);
    parts.push("");
    if (entry.data.description) {
      parts.push(`> ${entry.data.description}`);
      parts.push("");
    }
    parts.push(body);
    parts.push("");
  }

  return new Response(parts.join("\n") + "\n", {
    headers: { "Content-Type": "text/plain; charset=utf-8" },
  });
};
