"""Writes the small .xls fixtures for the max_cells tests (needs xlwt):
nix shell --impure --expr '(import (builtins.getFlake "nixpkgs") {}).python3.withPackages (p: [p.xlwt])' -c python3 -I tests/fixtures/make_xls.py"""
import os
import xlwt

out = os.path.join(os.path.dirname(os.path.abspath(__file__)), "sheets")
os.makedirs(out, exist_ok=True)


def book(name, sheets):
    wb = xlwt.Workbook()
    for title, cells in sheets:
        ws = wb.add_sheet(title)
        for r, c in cells:
            ws.write(r, c, "x")
    wb.save(os.path.join(out, name))


# a 10x100 rectangle (1,000 cells) from only two written cells
book("big.xls", [("S", [(0, 0), (99, 9)])])
# two sheets of 20x30 = 600 cells each
book("two.xls", [("A", [(0, 0), (29, 19)]), ("B", [(0, 0), (29, 19)])])
book("small.xls", [("S", [(0, 0), (1, 1)])])
