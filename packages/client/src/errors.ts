export class TaskFastError extends Error {
  readonly status: number;
  readonly body: unknown;
  constructor(name: string, status: number, body: unknown, message?: string) {
    super(message ?? `${name} (HTTP ${status})`);
    this.name = name;
    this.status = status;
    this.body = body;
  }
}

export class AuthError extends TaskFastError {
  constructor(status: number, body: unknown) {
    super("AuthError", status, body);
  }
}

export class ValidationError extends TaskFastError {
  readonly errorCode: string | undefined;
  constructor(status: number, body: unknown) {
    super("ValidationError", status, body);
    this.errorCode = extractErrorCode(body);
  }
}

function extractErrorCode(body: unknown): string | undefined {
  if (body && typeof body === "object" && "error_code" in body) {
    const code = (body as { error_code: unknown }).error_code;
    return typeof code === "string" ? code : undefined;
  }
  return undefined;
}
