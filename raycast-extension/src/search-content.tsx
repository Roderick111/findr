import { Action, ActionPanel, List, Icon, Keyboard } from "@raycast/api";
import { useExec } from "@raycast/utils";
import { useState, useMemo } from "react";
import { SearchResponse } from "./types";
import { getFindrPath, getMaxResults, getFileIcon } from "./utils";

export default function SearchContent() {
  const [query, setQuery] = useState("");
  const findrPath = getFindrPath();
  const maxResults = getMaxResults();

  const { isLoading, data } = useExec(
    findrPath,
    ["search", query, "--content", "--json", "--limit", String(maxResults)],
    {
      execute: query.length > 1,
      keepPreviousData: true,
      parseOutput: ({ stdout }) => {
        try {
          return JSON.parse(stdout) as SearchResponse;
        } catch {
          return null;
        }
      },
    },
  );

  const results = useMemo(() => data?.results || [], [data]);
  const elapsed = data?.elapsed_ms ?? 0;

  return (
    <List
      isLoading={isLoading && query.length > 1}
      isShowingDetail
      searchBarPlaceholder="Search inside file contents (PDFs, text, code)..."
      onSearchTextChange={setQuery}
      throttle
    >
      {query.length <= 1 ? (
        <List.EmptyView
          icon={Icon.Document}
          title="Type to search file contents"
          description="Searches inside PDFs, text files, and source code"
        />
      ) : results.length === 0 && !isLoading ? (
        <List.EmptyView
          icon={Icon.XMarkCircle}
          title="No content matches"
          description={`Nothing found for "${query}" inside file contents.`}
        />
      ) : (
        <List.Section
          title={`${results.length} content matches`}
          subtitle={`${elapsed}ms`}
        >
          {results.map((result, index) => (
            <List.Item
              key={`${result.path}-${index}`}
              icon={getFileIcon(result.file_type)}
              title={result.filename}
              subtitle={result.file_type || ""}
              detail={
                <List.Item.Detail
                  markdown={formatDetailMarkdown(
                    result.filename,
                    result.path,
                    result.content_snippet,
                  )}
                  metadata={
                    <List.Item.Detail.Metadata>
                      <List.Item.Detail.Metadata.Label
                        title="Path"
                        text={result.path}
                      />
                      <List.Item.Detail.Metadata.Label
                        title="Type"
                        text={result.file_type || "unknown"}
                      />
                      <List.Item.Detail.Metadata.Label
                        title="Score"
                        text={String(Math.round(result.score * 100) / 100)}
                      />
                    </List.Item.Detail.Metadata>
                  }
                />
              }
              actions={
                <ActionPanel>
                  <Action.Open title="Open File" target={result.path} />
                  <Action.ShowInFinder path={result.path} />
                  <Action.CopyToClipboard
                    title="Copy Path"
                    content={result.path}
                    shortcut={Keyboard.Shortcut.Common.Copy}
                  />
                  <Action.CopyToClipboard
                    title="Copy Filename"
                    content={result.filename}
                    shortcut={{ modifiers: ["cmd", "shift"], key: "c" }}
                  />
                </ActionPanel>
              }
            />
          ))}
        </List.Section>
      )}
    </List>
  );
}

function formatDetailMarkdown(
  filename: string,
  path: string,
  snippet: string | null,
): string {
  let md = `## ${filename}\n\n`;
  md += `\`${path}\`\n\n`;
  if (snippet) {
    md += `---\n\n### Content Match\n\n> ${snippet}\n`;
  }
  return md;
}
