// Turns the bridge's live activity events into what the Live view shows:
// feed rows, the "right now" caption, and connected mail apps. Pure functions,
// no React.
import type { ActivityEvent, ClientInfo } from "./types";

/** `tuta`: the bridge talked to Tuta. `local`: it stayed on this device. */
export type Tone = "tuta" | "local";
export type Icon = "mail" | "send" | "sync" | "check" | "move" | "trash" | "download";

export interface Caption {
  title: string;
  sub: string;
}

export interface FeedItem {
  id: number;
  key: string;
  icon: Icon;
  tone: Tone;
  title: string;
  meta: string;
  at: number;
  /** What repeated events add up to (mails received, marked read...). */
  total: number;
}

/** A row repeats into the previous one within this window. */
const MERGE_WINDOW_MS = 60_000;
const FEED_LENGTH = 50;

export function folderLabel(path: string): string {
  if (path.toUpperCase() === "INBOX") return "Inbox";
  return path.split("/").pop() || path;
}

function appName(id: number, clients: ClientInfo[]): string {
  return clients.find((c) => c.id === id)?.name ?? "Your mail app";
}

function mails(n: number): string {
  return n === 1 ? "1 mail" : `${n} mails`;
}

/** The client behind an event, if any: its chip lights up. */
export function eventClient(e: ActivityEvent): number | null {
  return "client" in e ? e.client : null;
}

export function eventTone(e: ActivityEvent): Tone {
  return e.kind === "synced" ? "local" : "tuta";
}

function eventKey(e: ActivityEvent): string {
  switch (e.kind) {
    case "synced":
      return `synced:${e.client}:${e.folder}`;
    case "marked_read":
      return `marked_read:${e.read}`;
    case "moved":
      return `moved:${e.folder}`;
    case "new_mail":
      return `new_mail:${e.folder}`;
    default:
      return e.kind;
  }
}

/** How much one event adds to its row; a sync replaces instead. */
function eventAmount(e: ActivityEvent): number {
  switch (e.kind) {
    case "synced":
      return e.messages;
    case "marked_read":
    case "moved":
    case "deleted":
      return e.count;
    default:
      return 1;
  }
}

interface Rendered {
  icon: Icon;
  title: string;
  meta: string;
  caption: Caption;
}

function render(e: ActivityEvent, clients: ClientInfo[], total: number): Rendered {
  const synced = "Synced to your Tuta account";
  switch (e.kind) {
    case "synced": {
      const app = appName(e.client, clients);
      const folder = folderLabel(e.folder);
      return {
        icon: "sync",
        title: `${app} synced ${folder}`,
        meta: `${total === 1 ? "1 message" : `${total} messages`} from the local cache`,
        caption: { title: `${app} synced ${folder}`, sub: "Served from this device" },
      };
    }
    case "body_downloaded":
      return {
        icon: "download",
        title: total === 1 ? "Older mail downloaded" : `${total} older mails downloaded`,
        meta: "Not on this device yet, fetched from Tuta",
        caption: { title: "Opening an older mail", sub: "Downloading it from Tuta" },
      };
    case "marked_read": {
      const title = `${mails(total)} marked as ${e.read ? "read" : "unread"}`;
      return { icon: "check", title, meta: synced, caption: { title, sub: synced } };
    }
    case "moved": {
      const title = `${mails(total)} moved to ${folderLabel(e.folder)}`;
      return { icon: "move", title, meta: synced, caption: { title, sub: synced } };
    }
    case "deleted": {
      const title = `${mails(total)} deleted`;
      return { icon: "trash", title, meta: synced, caption: { title, sub: synced } };
    }
    case "new_mail": {
      const folder = folderLabel(e.folder);
      return {
        icon: "mail",
        title: total === 1 ? `New mail in ${folder}` : `${total} new mails in ${folder}`,
        meta: clients.length > 0 ? "Pushed to your mail apps" : "Ready for your mail apps",
        caption: { title: "New mail delivered", sub: `Decrypted on this device, in ${folder}` },
      };
    }
    case "sent":
      return {
        icon: "send",
        title: total === 1 ? "Mail sent" : `${total} mails sent`,
        meta: "Encrypted and handed to Tuta",
        caption: { title: "Mail sent", sub: "Encrypted and handed to Tuta" },
      };
  }
}

export function captionFor(e: ActivityEvent, clients: ClientInfo[]): Caption {
  return render(e, clients, eventAmount(e)).caption;
}

/** The feed with `e` added on top, merged into the top row when it repeats. */
export function pushFeed(
  feed: FeedItem[],
  e: ActivityEvent,
  clients: ClientInfo[],
  now: number,
  nextId: number,
): FeedItem[] {
  const key = eventKey(e);
  const top = feed[0];
  const merge = top !== undefined && top.key === key && now - top.at < MERGE_WINDOW_MS;
  const total = merge && e.kind !== "synced" ? top.total + eventAmount(e) : eventAmount(e);
  const { icon, title, meta } = render(e, clients, total);
  const item: FeedItem = {
    id: merge ? top.id : nextId,
    key,
    icon,
    tone: eventTone(e),
    title,
    meta,
    at: now,
    total,
  };
  return merge ? [item, ...feed.slice(1)] : [item, ...feed].slice(0, FEED_LENGTH);
}

/** One mail app, which may hold several IMAP connections. */
export interface ClientGroup {
  key: string;
  name: string;
  version: string | null;
  os: string | null;
  address: string;
  connections: number;
  since: number;
  folders: string[];
  ids: number[];
}

function hostOf(peer: string): string {
  const at = peer.lastIndexOf(":");
  return (at > 0 ? peer.slice(0, at) : peer).replace(/^\[|\]$/g, "");
}

export function groupClients(clients: ClientInfo[]): ClientGroup[] {
  const groups = new Map<string, ClientGroup>();
  for (const c of clients) {
    const name = c.name ?? "Mail app";
    const address = hostOf(c.peer);
    const key = `${name}|${address}`;
    const g = groups.get(key);
    const folder = c.folder ? folderLabel(c.folder) : null;
    if (g) {
      g.connections += 1;
      g.since = Math.min(g.since, c.connected_at_ms);
      g.version ??= c.version;
      g.os ??= c.os;
      if (folder && !g.folders.includes(folder)) g.folders.push(folder);
      g.ids.push(c.id);
    } else {
      groups.set(key, {
        key,
        name,
        version: c.version,
        os: c.os,
        address,
        connections: 1,
        since: c.connected_at_ms,
        folders: folder ? [folder] : [],
        ids: [c.id],
      });
    }
  }
  return [...groups.values()];
}

export function ago(at: number, now: number): string {
  const sec = Math.max(0, Math.round((now - at) / 1000));
  if (sec < 5) return "now";
  if (sec < 60) return `${sec}s ago`;
  const min = Math.round(sec / 60);
  if (min < 60) return `${min} min ago`;
  return `${Math.round(min / 60)} h ago`;
}

export function clockTime(ms: number): string {
  return new Date(ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}
