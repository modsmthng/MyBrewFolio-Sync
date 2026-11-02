// SPDX-License-Identifier: GPL-3.0-or-later
export async function request(path, body, csrfToken) {
  const response = await fetch(`./api/${path}`, {
    method: body ? 'POST' : 'GET',
    credentials: 'same-origin',
    cache: 'no-store',
    headers: body ? { 'Content-Type': 'application/json', 'X-MyBrewFolio-CSRF': csrfToken } : {},
    body: body ? JSON.stringify(body) : undefined,
  });
  if (!response.ok) {
    const data = await response.json().catch(() => ({}));
    throw new Error(data.error || (response.status === 403
      ? 'Your Home Assistant session changed. Reload this page and try again.'
      : 'The request failed. Please try again.'));
  }
  return response.json();
}

// Only the cloud pairing page may be opened from a server-provided URL.
export function pairingLink(value) {
  try {
    const url = new URL(value);
    return url.origin === 'https://mybrewfolio.com' && url.pathname === '/v1/sync/device-auth/authorize'
      ? url.href : null;
  } catch { return null; }
}
