/**
 * Thin fetch wrapper for the HomeTube JSON API.
 *
 * Sends/receives JSON, throws on non-2xx responses, and includes
 * credentials so session cookies are sent.
 */

export class ApiError extends Error {
  constructor(
    public status: number,
    public body: unknown,
    message: string,
  ) {
    super(message);
    this.name = 'ApiError';
  }
}

async function request<T>(
  path: string,
  init: RequestInit & { json?: unknown } = {},
): Promise<T> {
  const { json, headers, ...rest } = init;

  const finalHeaders: Record<string, string> = {
    Accept: 'application/json',
    ...((headers as Record<string, string>) ?? {}),
  };
  let body = rest.body;
  if (json !== undefined) {
    finalHeaders['Content-Type'] = 'application/json';
    body = JSON.stringify(json);
  }

  const res = await fetch(path, {
    ...rest,
    body,
    headers: finalHeaders,
    credentials: 'same-origin',
  });

  let data: unknown = null;
  const contentType = res.headers.get('content-type') ?? '';
  if (contentType.includes('application/json')) {
    data = await res.json().catch(() => null);
  } else {
    data = await res.text().catch(() => null);
  }

  if (!res.ok) {
    throw new ApiError(res.status, data, `HTTP ${res.status} on ${path}`);
  }
  return data as T;
}

export const api = {
  get: <T>(path: string) => request<T>(path, { method: 'GET' }),
  post: <T>(path: string, json?: unknown) =>
    request<T>(path, { method: 'POST', json }),
  put: <T>(path: string, json?: unknown) =>
    request<T>(path, { method: 'PUT', json }),
  delete: <T>(path: string) => request<T>(path, { method: 'DELETE' }),
};
