"""appflowy-cli: standalone entry point for the AppFlowy CLI.

All functionality lives in appflowy-mcp (appflowy_mcp.cli); this package
just gives the CLI its own name on PyPI so `uvx appflowy-cli` works.
"""
from appflowy_mcp.cli import main

__all__ = ["main"]
