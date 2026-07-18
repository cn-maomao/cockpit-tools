import importlib
import sys
import types
import unittest
from pathlib import Path


MODULE_DIR = Path(__file__).resolve().parents[1]
if str(MODULE_DIR) not in sys.path:
    sys.path.insert(0, str(MODULE_DIR))

try:
    import curl_cffi  # noqa: F401
except ModuleNotFoundError:
    curl_cffi_stub = types.ModuleType("curl_cffi")
    curl_cffi_stub.requests = types.SimpleNamespace()
    sys.modules["curl_cffi"] = curl_cffi_stub

mail_service = importlib.import_module("mail_service")
app_config = importlib.import_module("app_config")


class FakeResponse:
    def __init__(self, payload):
        self._payload = payload
        self.text = ""

    def raise_for_status(self):
        return None

    def json(self):
        return self._payload


class CloudMailAuthenticationTests(unittest.TestCase):
    def setUp(self):
        mail_service.config = {
            "cloudmail_api_base": "https://mail.example.com",
            "cloudmail_admin_email": "admin@example.com",
            "cloudmail_admin_password": "secret",
        }
        mail_service._cloudmail_public_token = None
        self.calls = []

        def fake_post(url, **kwargs):
            self.calls.append((url, kwargs))
            token = f"token-{len(self.calls)}"
            return FakeResponse({"code": 200, "data": {"token": token}})

        mail_service.http_post = fake_post

    def test_admin_credentials_generate_and_cache_public_token(self):
        first = mail_service.get_cloudmail_public_token()
        second = mail_service.get_cloudmail_public_token()

        self.assertEqual(first, "token-1")
        self.assertEqual(second, "token-1")
        self.assertEqual(len(self.calls), 1)
        url, kwargs = self.calls[0]
        self.assertEqual(url, "https://mail.example.com/api/public/genToken")
        self.assertEqual(
            kwargs["json"],
            {"email": "admin@example.com", "password": "secret"},
        )
        self.assertEqual(kwargs["proxies"], {})

    def test_force_refresh_replaces_cached_token(self):
        self.assertEqual(mail_service.get_cloudmail_public_token(), "token-1")
        self.assertEqual(
            mail_service.get_cloudmail_public_token(force_refresh=True),
            "token-2",
        )
        self.assertEqual(len(self.calls), 2)

    def test_cloudmail_requires_admin_credentials_not_legacy_token(self):
        config = app_config.DEFAULT_CONFIG.copy()
        config.update(
            {
                "email_provider": "cloudmail",
                "cloudmail_api_base": "https://mail.example.com",
                "cloudmail_public_token": "legacy-token",
                "cloudmail_domains": "example.com",
            }
        )

        with self.assertRaises(app_config.ConfigError) as context:
            app_config.validate_run_requirements(config)

        self.assertIn("cloudmail_admin_email", str(context.exception))
        self.assertIn("cloudmail_admin_password", str(context.exception))


if __name__ == "__main__":
    unittest.main()
