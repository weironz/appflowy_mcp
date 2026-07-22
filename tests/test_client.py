"""Offline tests for the HTTP client's retry / reauth / error handling."""

import pytest

from appflowy_mcp import client as cm
from appflowy_mcp.client import AppFlowyClient, AppFlowyError


class Resp:
    def __init__(self, status, body=None, headers=None, text=""):
        self.status_code = status
        self._body = body
        self.headers = headers or {}
        self.text = text

    def json(self):
        if self._body is None:
            raise ValueError("no json body")
        return self._body


class Http:
    def __init__(self, responses):
        self.responses = list(responses)
        self.calls = 0
        self.methods = []

    def request(self, method, url, headers=None, params=None, json=None, content=None):
        self.calls += 1
        self.methods.append(method)
        return self.responses.pop(0)


def make_client(responses, monkeypatch):
    monkeypatch.setattr(cm.time, "sleep", lambda *a, **k: None)
    c = AppFlowyClient(email="e", password="p")
    c.token_store.set_access_token("t")
    c.token_store.set_refresh_token("r")
    c._http_client = Http(responses)
    return c


def test_get_retried_on_5xx(monkeypatch):
    c = make_client([Resp(500, {"message": "boom"}), Resp(200, {"data": "ok"})], monkeypatch)
    out = c._request("GET", "/x")
    assert out["data"] == "ok"
    assert c._http_client.calls == 2


def test_post_not_retried_on_5xx(monkeypatch):
    c = make_client([Resp(500, {"message": "boom"})], monkeypatch)
    with pytest.raises(AppFlowyError) as e:
        c._request("POST", "/x", json_body={})
    assert c._http_client.calls == 1  # not retried -> no duplicate write
    assert e.value.status == 500


def test_post_retried_on_429(monkeypatch):
    # 429 means the request was rejected, not applied, so retry is safe for POST.
    c = make_client(
        [Resp(429, {"message": "rate"}, {"retry-after": "0"}), Resp(200, {"data": "ok"})],
        monkeypatch,
    )
    out = c._request("POST", "/x", json_body={})
    assert out["data"] == "ok"
    assert c._http_client.calls == 2


def test_401_triggers_reauth_and_retry(monkeypatch):
    c = make_client([Resp(401, {"message": "expired"}), Resp(200, {"data": "ok"})], monkeypatch)
    c.refresh_token = lambda: c.token_store.set_access_token("new")
    out = c._request("GET", "/x")
    assert out["data"] == "ok"
    assert c.token_store.get_access_token() == "new"
    assert c._http_client.calls == 2


def test_error_preserves_status_and_code(monkeypatch):
    c = make_client([Resp(400, {"message": "bad", "code": 1026})], monkeypatch)
    with pytest.raises(AppFlowyError) as e:
        c._request("GET", "/x")
    assert e.value.status == 400
    assert e.value.code == 1026


def test_business_error_200_envelope(monkeypatch):
    c = make_client([Resp(200, {"code": 1026, "message": "limit"})], monkeypatch)
    with pytest.raises(AppFlowyError) as e:
        c._request("GET", "/x")
    assert e.value.code == 1026


def test_reauth_skips_when_token_already_refreshed():
    c = AppFlowyClient(email="e", password="p")
    c.token_store.set_access_token("already-refreshed")
    refreshed = {"n": 0}
    c.refresh_token = lambda: refreshed.__setitem__("n", refreshed["n"] + 1)
    # token seen at request time was "t", but it is now "already-refreshed" ->
    # another thread refreshed, so this call must not refresh again.
    assert c._reauthenticate("t") is True
    assert refreshed["n"] == 0
