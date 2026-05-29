import { clsx, type ClassValue } from "clsx";
import { twMerge } from "tailwind-merge";

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs));
}

/** Compact duration: 9s · 1m 5s · 1h 1m · 1d 1h (max two units). */
export function formatDuration(secs: number): string {
  secs = Math.max(0, Math.floor(secs));
  if (secs < 60) return `${secs}s`;
  const m = Math.floor(secs / 60);
  if (m < 60) return `${m}m ${secs % 60}s`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ${m % 60}m`;
  const d = Math.floor(h / 24);
  return `${d}d ${h % 24}h`;
}

/** Server uptime as Nd HH:MM:SS. */
export function formatUptime(secs: number): string {
  secs = Math.max(0, Math.floor(secs));
  const d = Math.floor(secs / 86400);
  const h = Math.floor((secs % 86400) / 3600);
  const m = Math.floor((secs % 3600) / 60);
  const s = secs % 60;
  const hms = [h, m, s].map((n) => String(n).padStart(2, "0")).join(":");
  return d > 0 ? `${d}d ${hms}` : hms;
}

/** The Toki band: 446.00–448.00 MHz in 0.05 steps → 41 channels. */
export const ALL_FREQUENCIES: string[] = Array.from({ length: 41 }, (_, i) =>
  (446.0 + i * 0.05).toFixed(2),
);

/** 1-based channel number for a canonical frequency string, or 0. */
export function channelNumber(freq: string): number {
  const idx = ALL_FREQUENCIES.indexOf(freq);
  return idx < 0 ? 0 : idx + 1;
}
