import { isPermissionGranted, requestPermission, sendNotification } from "@tauri-apps/plugin-notification";

import { t } from "./strings";
import type { Notification } from "./types";

export function describe(n: Notification): { title: string; body: string } {
  switch (n.kind) {
    case "kit-updated":
      return { title: t("notifKitUpdated").replace("{kit}", n.kit), body: n.items.join(", ") };
    case "new-items":
      return {
        title: t("notifNewItems").replace("{kit}", n.kit),
        body: t("notifNewItemsBody").replace("{items}", n.items.map((i) => i.id).join(", ")),
      };
    case "restart-needed":
      return { title: t("notifRestart").replace("{clients}", n.clients.join(", ")), body: t("notifRestartBody") };
    case "app-updated":
      return { title: t("notifAppUpdated").replace("{v}", n.version), body: "" };
  }
}

export async function notify(n: Notification): Promise<void> {
  const { title, body } = describe(n);
  try {
    const granted = (await isPermissionGranted()) || (await requestPermission()) === "granted";
    if (granted) sendNotification({ title, body });
  } catch {
    // No notification center: the in-app journal still has the entry.
  }
}
