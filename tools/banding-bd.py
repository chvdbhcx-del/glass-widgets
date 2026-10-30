"""Backdrops for the banding probes: flat black, flat dark blue-grey (out/bd/black.png, dark.png)."""
import os
from PIL import Image
os.makedirs("out/bd", exist_ok=True)
Image.new("RGB", (1920, 1080), (0, 0, 0)).save("out/bd/black.png")
Image.new("RGB", (1920, 1080), (11, 12, 15)).save("out/bd/dark.png")
