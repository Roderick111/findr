// Minimal repro for Quick Look crash when quickLook + detail used on same List.Item
import { List, Action, ActionPanel } from "@raycast/api";
import { useEffect } from "react";
import { writeFileSync } from "fs";

const ITEMS = ["alpha", "bravo", "charlie", "delta", "echo"];
const QL_PATH = "/tmp/ql-test.txt";

export default function Command() {
  useEffect(() => {
    writeFileSync(QL_PATH, "Quick Look test content.\nLine two.\n");
  }, []);

  return (
    <List isShowingDetail={true}>
      {ITEMS.map((name) => (
        <List.Item
          key={name}
          title={name}
          quickLook={{ path: QL_PATH, name: "test.txt" }}
          detail={<List.Item.Detail markdown="**Detail panel content**" />}
          actions={
            <ActionPanel>
              <Action.ToggleQuickLook />
            </ActionPanel>
          }
        />
      ))}
    </List>
  );
}
