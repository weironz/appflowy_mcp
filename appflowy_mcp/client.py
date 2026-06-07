import time
from dataclasses import dataclass
from typing import Any
from urllib.parse import quote

import httpx


DEFAULT_BASE_URL = "https://beta.appflowy.cloud"

# Max row ids per row/detail request, to keep the `ids` query string well under
# common URL-length limits when a database has many rows.
ROW_DETAIL_BATCH_SIZE = 100


@dataclass
class TokenResponse:
    access_token: str
    refresh_token: str
    expires_in: int | None = None


class TokenStore:
    def __init__(self) -> None:
        self._access_token: str | None = None
        self._refresh_token: str | None = None

    def get_access_token(self) -> str | None:
        return self._access_token

    def set_access_token(self, access_token: str | None) -> None:
        self._access_token = access_token

    def get_refresh_token(self) -> str | None:
        return self._refresh_token

    def set_refresh_token(self, refresh_token: str | None) -> None:
        self._refresh_token = refresh_token

    def set_token_response(self, token: TokenResponse) -> None:
        self._access_token = token.access_token
        self._refresh_token = token.refresh_token


class AppFlowyClient:
    def __init__(
        self,
        email: str | None = None,
        password: str | None = None,
        base_url: str = DEFAULT_BASE_URL,
    ) -> None:
        self.email = email
        self.password = password
        self.base_url = base_url.rstrip("/")
        self.token_store = TokenStore()
        self._http_client = httpx.Client(timeout=60.0)
        self._max_retries = 3
        self._backoff_base = 0.5

    def login(self) -> TokenResponse:
        if not self.email or not self.password:
            raise Exception("Email and password are required for login.")

        body = self._request(
            "POST",
            "/gotrue/token?grant_type=password",
            json_body={"email": self.email, "password": self.password},
            allow_reauth=False,
        )
        token = TokenResponse(
            access_token=body["access_token"],
            refresh_token=body["refresh_token"],
            expires_in=body.get("expires_in"),
        )
        self.token_store.set_token_response(token)
        return token

    def refresh_token(self) -> TokenResponse:
        refresh_token = self.token_store.get_refresh_token()
        if not refresh_token:
            raise Exception("No refresh token available. Please login first.")

        body = self._request(
            "POST",
            "/gotrue/token?grant_type=refresh_token",
            json_body={"refresh_token": refresh_token},
            allow_reauth=False,
        )
        token = TokenResponse(
            access_token=body["access_token"],
            refresh_token=body["refresh_token"],
            expires_in=body.get("expires_in"),
        )
        self.token_store.set_token_response(token)
        return token

    def get_database_fields(
        self,
        workspace_id: str,
        database_id: str,
    ) -> list[dict[str, Any]]:
        body = self._request(
            "GET",
            f"/api/workspace/{workspace_id}/database/{database_id}/fields",
        )
        return body.get("data", [])

    def get_database_row_ids(
        self,
        workspace_id: str,
        database_id: str,
    ) -> list[dict[str, Any]]:
        body = self._request(
            "GET",
            f"/api/workspace/{workspace_id}/database/{database_id}/row",
        )
        return body.get("data", [])

    def get_database_row_details(
        self,
        workspace_id: str,
        database_id: str,
        row_ids: list[str],
        *,
        with_doc: bool | None = None,
    ) -> list[dict[str, Any]]:
        if not row_ids:
            raise Exception("At least one row ID is required.")

        results: list[dict[str, Any]] = []
        for start in range(0, len(row_ids), ROW_DETAIL_BATCH_SIZE):
            chunk = row_ids[start : start + ROW_DETAIL_BATCH_SIZE]
            params: dict[str, Any] = {"ids": ",".join(chunk)}
            if with_doc is not None:
                params["with_doc"] = with_doc

            body = self._request(
                "GET",
                f"/api/workspace/{workspace_id}/database/{database_id}/row/detail",
                params=params,
            )
            results.extend(body.get("data", []))
        return results

    def create_database_row(
        self,
        workspace_id: str,
        database_id: str,
        *,
        cells: dict[str, Any] | None = None,
        document: str | None = None,
    ) -> str:
        body = self._request(
            "POST",
            f"/api/workspace/{workspace_id}/database/{database_id}/row",
            json_body={"cells": cells or {}, "document": document},
        )
        return str(body.get("data", ""))

    def upsert_database_row(
        self,
        workspace_id: str,
        database_id: str,
        pre_hash: str,
        *,
        cells: dict[str, Any] | None = None,
        document: str | None = None,
    ) -> str:
        body = self._request(
            "PUT",
            f"/api/workspace/{workspace_id}/database/{database_id}/row",
            json_body={
                "pre_hash": pre_hash,
                "cells": cells or {},
                "document": document,
            },
        )
        return str(body.get("data", ""))

    def get_database_row_ids_updated(
        self,
        workspace_id: str,
        database_id: str,
        *,
        after: str | None = None,
    ) -> list[dict[str, Any]]:
        params: dict[str, Any] = {}
        if after is not None:
            params["after"] = after

        body = self._request(
            "GET",
            f"/api/workspace/{workspace_id}/database/{database_id}/row/updated",
            params=params,
        )
        return body.get("data", [])

    def upload_file_blob_v1(
        self,
        workspace_id: str,
        parent_dir: str,
        *,
        content: bytes,
        content_type: str = "application/octet-stream",
    ) -> dict[str, Any]:
        encoded_parent_dir = quote(parent_dir, safe="")
        body = self._request_content_json(
            "PUT",
            f"/api/file_storage/{workspace_id}/v1/blob/{encoded_parent_dir}",
            content=content,
            content_type=content_type,
        )
        data = body.get("data", body)
        if not isinstance(data, dict):
            raise Exception("Expected file upload response data to be an object.")
        file_id = data.get("file_id")
        if not file_id:
            raise Exception("File upload response did not include file_id.")
        return {
            **data,
            "workspace_id": workspace_id,
            "parent_dir": parent_dir,
            "url": self.file_blob_url_v1(workspace_id, parent_dir, str(file_id)),
            "content_type": content_type,
            "content_length": len(content),
        }

    def file_blob_url_v1(self, workspace_id: str, parent_dir: str, file_id: str) -> str:
        encoded_parent_dir = quote(parent_dir, safe="")
        encoded_file_id = quote(file_id, safe="")
        return (
            f"{self.base_url}/api/file_storage/{workspace_id}/v1/blob/"
            f"{encoded_parent_dir}/{encoded_file_id}"
        )

    def _request(
        self,
        method: str,
        path: str,
        params: dict[str, Any] | None = None,
        json_body: dict[str, Any] | list[Any] | None = None,
        *,
        allow_reauth: bool = True,
    ) -> dict[str, Any]:
        # Some endpoints (remove members, invite) take a top-level JSON array;
        # only dict bodies get the drop-None-values treatment.
        json_payload = (
            {k: v for k, v in json_body.items() if v is not None}
            if isinstance(json_body, dict)
            else json_body
        )

        def build() -> httpx.Response:
            return self._http_client.request(
                method=method,
                url=f"{self.base_url}{path}",
                headers=self._headers(),
                params=params,
                json=json_payload,
            )

        return self._handle_response(self._send(build, allow_reauth=allow_reauth))

    def _request_content_json(
        self,
        method: str,
        path: str,
        *,
        content: bytes,
        content_type: str,
        allow_reauth: bool = True,
    ) -> dict[str, Any]:
        def build() -> httpx.Response:
            headers = self._headers()
            headers["Content-Type"] = content_type
            return self._http_client.request(
                method=method,
                url=f"{self.base_url}{path}",
                headers=headers,
                content=content,
            )

        return self._handle_response(self._send(build, allow_reauth=allow_reauth))

    def _send(self, build, *, allow_reauth: bool) -> httpx.Response:
        """Send a request with one auto-reauth on 401 and backoff on 429/5xx.

        `build` must (re)build and send the request each call, reading a fresh
        Authorization header, so a token refreshed mid-flight is picked up.
        """
        attempt = 0
        reauthed = False
        while True:
            response = build()
            status = response.status_code

            if status == 401 and allow_reauth and not reauthed:
                reauthed = True
                if self._reauthenticate():
                    continue

            if (status == 429 or status >= 500) and attempt < self._max_retries:
                time.sleep(self._retry_delay(response, attempt))
                attempt += 1
                continue

            return response

    def _reauthenticate(self) -> bool:
        """Refresh the access token, falling back to a fresh login."""
        try:
            if self.token_store.get_refresh_token():
                self.refresh_token()
                return True
        except Exception:
            pass
        try:
            if self.email and self.password:
                self.login()
                return True
        except Exception:
            pass
        return False

    def _retry_delay(self, response: httpx.Response, attempt: int) -> float:
        retry_after = response.headers.get("retry-after")
        if retry_after:
            try:
                return float(retry_after)
            except ValueError:
                pass
        return self._backoff_base * (2**attempt)

    def _headers(self) -> dict[str, str]:
        headers = {
            "Content-Type": "application/json",
            "Accept": "application/json",
        }
        access_token = self.token_store.get_access_token()
        if access_token:
            headers["Authorization"] = f"Bearer {access_token}"
        return headers

    def _handle_response(self, response: httpx.Response) -> dict[str, Any]:
        try:
            body = response.json()
        except Exception as e:
            raise Exception(
                f"Failed to parse response body: HTTP {response.status_code}"
            ) from e

        if response.status_code >= 400:
            message = (
                body.get("message", f"HTTP {response.status_code}")
                if isinstance(body, dict)
                else response.text
            )
            raise Exception(f"{message} (HTTP {response.status_code})")

        # AppFlowy returns business errors as HTTP 200 with a non-zero code in
        # the {code, message, data} envelope (e.g. 1026 workspace limit).
        if isinstance(body, dict) and body.get("code") not in (None, 0):
            raise Exception(
                f"{body.get('message', 'AppFlowy error')} (code {body['code']})"
            )

        return body if isinstance(body, dict) else {"data": body}
