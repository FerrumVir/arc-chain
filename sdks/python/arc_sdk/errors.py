from __future__ import annotations


class ArcError(Exception):
    def __init__(self, message: str, status_code: int | None = None, detail: str | None = None):
        self.message = message
        self.status_code = status_code
        self.detail = detail
        super().__init__(message)

    def __repr__(self) -> str:
        parts = [f"ArcError({self.message!r}"]
        if self.status_code is not None:
            parts.append(f", status_code={self.status_code}")
        if self.detail is not None:
            parts.append(f", detail={self.detail!r}")
        return "".join(parts) + ")"


class ArcConnectionError(ArcError):
    def __init__(self, message: str, url: str | None = None, cause: Exception | None = None):
        self.url = url
        self.cause = cause
        super().__init__(message)

    def __repr__(self) -> str:
        return f"ArcConnectionError({self.message!r}, url={self.url!r})"


class ArcTransactionError(ArcError):
    def __init__(
        self,
        message: str,
        tx_hash: str | None = None,
        status_code: int | None = None,
        detail: str | None = None,
    ):
        self.tx_hash = tx_hash
        super().__init__(message, status_code=status_code, detail=detail)

    def __repr__(self) -> str:
        return f"ArcTransactionError({self.message!r}, tx_hash={self.tx_hash!r})"


class ArcValidationError(ArcError):
    def __init__(self, message: str, field: str | None = None):
        self.field = field
        super().__init__(message)

    def __repr__(self) -> str:
        return f"ArcValidationError({self.message!r}, field={self.field!r})"


class ArcCryptoError(ArcError):
    def __init__(self, message: str, operation: str | None = None):
        self.operation = operation
        super().__init__(message)

    def __repr__(self) -> str:
        return f"ArcCryptoError({self.message!r}, operation={self.operation!r})"
