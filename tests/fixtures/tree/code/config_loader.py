import json


def parse_settings(path):
    """Read a JSON settings file and fill in defaults."""
    with open(path) as handle:
        settings = json.load(handle)
    settings.setdefault("retries", 3)
    settings.setdefault("timeout", 30)
    return settings
