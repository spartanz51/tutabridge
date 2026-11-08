import { useState, useEffect, useRef } from "react";
import type { ActivityEvent, Config, BridgeStatus, BridgeStats } from "../types";
import { isError } from "../types";
import {
  ago,
  captionFor,
  clockTime,
  eventClient,
  eventTone,
  groupClients,
  type Caption,
  type ClientGroup,
  type FeedItem,
  type Icon,
  type Tone,
} from "../activity";
import logo from "../assets/logo.png";

interface Props {
  status: BridgeStatus | null;
  stats: BridgeStats;
  config: Config | null;
  hasSavedSession: boolean | null;
  sessionWarning: string | null;
  loading: boolean;
  logs: string[];
  feed: FeedItem[];
  subscribeActivity: (listener: (event: ActivityEvent) => void) => () => void;
  needsTotp: boolean;
  onStart: (password?: string, email?: string) => Promise<void>;
  onSubmitTotp: (code: string) => Promise<void>;
}

export function Dashboard(props: Props) {
  const { status, needsTotp } = props;
  if (!status) {
    return (
      <div className="dashboard">
        <p className="muted">Connecting...</p>
      </div>
    );
  }
  const running = status === "Running";
  const starting = status === "Starting";
  if ((running || starting) && !needsTotp) {
    return <Live {...props} starting={starting} />;
  }
  return <SignIn {...props} status={status} />;
}

// --- Sign in (bridge stopped, or waiting for a 2FA code) ---

function SignIn({
  status,
  config,
  hasSavedSession,
  sessionWarning,
  loading,
  needsTotp,
  onStart,
  onSubmitTotp,
}: Props & { status: BridgeStatus }) {
  const [password, setPassword] = useState("");
  const [email, setEmail] = useState("");
  const [totp, setTotp] = useState("");

  const errored = isError(status);
  const isStopped = status === "Stopped" || errored;
  const needsEmail = isStopped && !config?.email;
  const needsPassword = isStopped && hasSavedSession === false;

  const handleStart = async () => {
    try {
      await onStart(
        needsPassword ? password : undefined,
        needsEmail ? email.trim() : undefined,
      );
    } finally {
      // The bridge keeps the session from here on: do not hold the password.
      setPassword("");
    }
  };

  const handleVerifyTotp = async () => {
    await onSubmitTotp(totp.trim());
    setTotp("");
  };

  const heading = needsTotp
    ? "Two-factor code"
    : needsEmail || needsPassword
      ? "Sign in to Tuta"
      : "Bridge stopped";
  const subtitle = needsTotp
    ? "Your account has 2FA. Enter the current code to finish signing in."
    : needsEmail || needsPassword
      ? "Then your mail apps can use your mailbox."
      : "Start the bridge to connect your mail apps.";

  return (
    <div className="signin">
      <form className="signin-form" onSubmit={(e) => e.preventDefault()}>
        <div className="signin-head">
          <div className="signin-logo">
            <span className="halo" aria-hidden="true" />
            <img src={logo} alt="" width={56} height={56} />
          </div>
          <h1>{heading}</h1>
          <p>{subtitle}</p>
        </div>

        {errored && <p className="error-text">{status.Error}</p>}
        {sessionWarning && <p className="field-hint" role="status">{sessionWarning}</p>}

        {needsTotp ? (
          <>
            <div className="field">
              <label htmlFor="totp">Code</label>
              <input
                id="totp"
                type="text"
                inputMode="numeric"
                value={totp}
                onChange={(e) => setTotp(e.target.value.replace(/\D/g, ""))}
                placeholder="6-digit code from your authenticator"
                autoFocus
                onKeyDown={(e) =>
                  e.key === "Enter" && totp.trim().length >= 6 && handleVerifyTotp()
                }
              />
            </div>
            <button
              type="button"
              className="primary big"
              onClick={handleVerifyTotp}
              disabled={totp.trim().length < 6}
            >
              Verify &amp; sign in
            </button>
          </>
        ) : (
          <>
            {needsEmail && (
              <div className="field">
                <label htmlFor="email">Email</label>
                <input
                  id="email"
                  type="email"
                  value={email}
                  onChange={(e) => setEmail(e.target.value)}
                  placeholder="you@tuta.com"
                  autoComplete="username"
                  autoFocus
                />
              </div>
            )}
            {needsPassword && (
              <div className="field">
                <label htmlFor="password">Password</label>
                <input
                  id="password"
                  type="password"
                  value={password}
                  onChange={(e) => setPassword(e.target.value)}
                  placeholder="Your Tuta password"
                  autoComplete="current-password"
                  onKeyDown={(e) =>
                    e.key === "Enter" &&
                    password &&
                    (!needsEmail || email.trim()) &&
                    handleStart()
                  }
                />
              </div>
            )}
            <button
              type="button"
              className="primary big"
              onClick={handleStart}
              disabled={
                loading ||
                (needsEmail && !email.trim()) ||
                (needsPassword && !password)
              }
            >
              {loading ? "Connecting…" : "Start bridge"}
            </button>
            {needsPassword && (
              <p className="signin-note">
                Your password is used once to sign in, then wiped from memory.
              </p>
            )}
          </>
        )}
      </form>
    </div>
  );
}

// --- Live (bridge running) ---

const IDLE: Caption = { title: "Up to date", sub: "Your mail apps and Tuta are in sync" };
const RECONNECTING: Caption = {
  title: "Reconnecting to Tuta",
  sub: "Your mail apps still work from this device",
};
const CAPTION_HOLD_MS = 3500;
const RIPPLE_MS = 1300;

function Live({ stats, logs, feed, subscribeActivity, starting }: Props & { starting: boolean }) {
  const [ripples, setRipples] = useState<{ id: number; tone: Tone }[]>([]);
  const [caption, setCaption] = useState<{ id: number; value: Caption } | null>(null);
  const [lit, setLit] = useState<Set<number>>(new Set());
  const [busy, setBusy] = useState(false);
  const [now, setNow] = useState(() => Date.now());
  const timers = useRef(new Set<number>());

  useEffect(() => {
    const tick = window.setInterval(() => setNow(Date.now()), 5000);
    const pending = timers.current;
    return () => {
      window.clearInterval(tick);
      pending.forEach((t) => window.clearTimeout(t));
    };
  }, []);

  // Each event: a ripple from the logo, the caption, and its app's chip.
  const clientsRef = useRef(stats.clients);
  useEffect(() => {
    clientsRef.current = stats.clients;
  }, [stats.clients]);
  useEffect(() => {
    let seq = 0;
    const later = (ms: number, fn: () => void) => {
      const timer = window.setTimeout(() => {
        timers.current.delete(timer);
        fn();
      }, ms);
      timers.current.add(timer);
    };
    return subscribeActivity((event) => {
      seq += 1;
      const id = seq;
      const tone = eventTone(event);
      setRipples((r) => [...r.slice(-3), { id, tone }]);
      later(RIPPLE_MS, () => setRipples((r) => r.filter((x) => x.id !== id)));
      if (tone === "tuta") {
        setBusy(true);
        later(700, () => setBusy(false));
      }
      setCaption({ id, value: captionFor(event, clientsRef.current) });
      later(CAPTION_HOLD_MS, () => setCaption((c) => (c?.id === id ? null : c)));
      const client = eventClient(event);
      if (client !== null) {
        setLit((s) => new Set(s).add(client));
        later(900, () =>
          setLit((s) => {
            const next = new Set(s);
            next.delete(client);
            return next;
          }),
        );
      }
    });
  }, [subscribeActivity]);

  const realtime =
    stats.ws_status === "Connected"
      ? "Connected"
      : stats.ws_status === "Stopped"
        ? "Off"
        : "Reconnecting";
  const shown: Caption = starting
    ? { title: "Starting", sub: logs[logs.length - 1] ?? "Signing in" }
    : (caption?.value ?? (realtime === "Connected" ? IDLE : RECONNECTING));
  const groups = groupClients(stats.clients);

  return (
    <div className="live">
      <section className="card hero" aria-label="Right now">
        <div className="pulse">
          <span className="idle-ring" aria-hidden="true" />
          {ripples.map((r) => (
            <span key={r.id} className={`ripple ripple-${r.tone}`} aria-hidden="true" />
          ))}
          <img
            className={busy || starting ? "pulse-logo busy" : "pulse-logo"}
            src={logo}
            alt=""
            width={60}
            height={60}
          />
        </div>
        <div className="hero-text">
          <div aria-live="polite">
            <div className="eyebrow">Right now</div>
            <div key={starting ? "starting" : (caption?.id ?? "idle")} className="caption">
              <div className="caption-title">{shown.title}</div>
              <div className="caption-sub">{shown.sub}</div>
            </div>
          </div>
          {!starting && (
            <div className="apps">
              <div className="apps-count">
                {groups.length === 0
                  ? "No mail app connected yet"
                  : groups.length === 1
                    ? "1 app connected"
                    : `${groups.length} apps connected`}
              </div>
              {groups.length > 0 && (
                <div className="chips">
                  {groups.map((g) => (
                    <ClientChip
                      key={g.key}
                      group={g}
                      lit={g.ids.some((id) => lit.has(id))}
                      now={now}
                    />
                  ))}
                </div>
              )}
            </div>
          )}
        </div>
      </section>

      <div className="live-bottom">
        <section className="card feed" aria-label="Activity">
          <h2>Activity</h2>
          {feed.length === 0 ? (
            <p className="feed-empty">Activity shows up here as your mail apps sync.</p>
          ) : (
            <ul>
              {feed.map((item) => (
                <li key={item.id} className="feed-row">
                  <span className={`feed-icon feed-icon-${item.tone}`}>
                    <FeedIcon icon={item.icon} />
                  </span>
                  <span className="feed-text">
                    <span className="feed-title">{item.title}</span>
                    <span className="feed-meta">{item.meta}</span>
                  </span>
                  <span className="feed-time">{ago(item.at, now)}</span>
                </li>
              ))}
            </ul>
          )}
        </section>

        <aside className="side" aria-label="Status">
          <div className="card side-card">
            <div className="side-label">Mailbox</div>
            <div className="side-value">{stats.mails_synced.toLocaleString()}</div>
            <div className="side-hint">mails, all folders</div>
          </div>
          <div className="card side-card side-rows">
            <div>
              <span>Realtime</span>
              <span className={realtime === "Connected" ? "ok" : "warn"}>{realtime}</span>
            </div>
            {stats.started_at_ms !== null && (
              <div>
                <span>Running since</span>
                <span>{clockTime(stats.started_at_ms)}</span>
              </div>
            )}
          </div>
        </aside>
      </div>
    </div>
  );
}

function ClientChip({ group, lit, now }: { group: ClientGroup; lit: boolean; now: number }) {
  const tipId = `client-${group.key.replace(/[^a-z0-9]/gi, "-")}`;
  const title = group.version ? `${group.name} ${group.version}` : group.name;
  return (
    <button type="button" className={lit ? "chip lit" : "chip"} aria-describedby={tipId}>
      <span className="chip-led" />
      {group.name}
      <span className="chip-tip" role="tooltip" id={tipId}>
        <strong>{title}</strong>
        {group.os && <span className="tip-os">{group.os}</span>}
        <span className="tip-row">
          <span>IMAP connections</span>
          <span>{group.connections}</span>
        </span>
        <span className="tip-row">
          <span>Address</span>
          <span className="mono">{group.address}</span>
        </span>
        <span className="tip-row">
          <span>Connected</span>
          <span>{ago(group.since, now)}</span>
        </span>
        {group.folders.length > 0 && (
          <span className="tip-row">
            <span>Open</span>
            <span>{group.folders.join(", ")}</span>
          </span>
        )}
      </span>
    </button>
  );
}

function FeedIcon({ icon }: { icon: Icon }) {
  const common = {
    width: 14,
    height: 14,
    viewBox: "0 0 24 24",
    fill: "none",
    stroke: "currentColor",
    strokeWidth: 1.8,
    strokeLinecap: "round" as const,
    strokeLinejoin: "round" as const,
    "aria-hidden": true,
  };
  switch (icon) {
    case "mail":
      return (
        <svg {...common}>
          <rect x="3" y="5" width="18" height="14" rx="2" />
          <path d="m3 7 9 6 9-6" />
        </svg>
      );
    case "send":
      return (
        <svg {...common}>
          <path d="M22 2 11 13" />
          <path d="M22 2 15 22l-4-9-9-4z" />
        </svg>
      );
    case "sync":
      return (
        <svg {...common}>
          <path d="M21 12a9 9 0 1 1-3-6.7L21 8" />
          <path d="M21 3v5h-5" />
        </svg>
      );
    case "check":
      return (
        <svg {...common}>
          <path d="M20 6 9 17l-5-5" />
        </svg>
      );
    case "move":
      return (
        <svg {...common}>
          <path d="M3 7h6l2 2h10v10H3z" />
          <path d="m13 14 3-3 3 3" />
        </svg>
      );
    case "trash":
      return (
        <svg {...common}>
          <path d="M4 7h16" />
          <path d="M9 7V4h6v3" />
          <path d="M6 7l1 13h10l1-13" />
        </svg>
      );
    case "download":
      return (
        <svg {...common}>
          <path d="M12 4v11" />
          <path d="m7 10 5 5 5-5" />
          <path d="M5 20h14" />
        </svg>
      );
  }
}
