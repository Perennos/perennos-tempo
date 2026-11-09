"""The Windows programs and the installer carry the official Perennos insignia, with Perennos in their version details.
Run: python -m unittest discover -s tests"""
import hashlib
import pathlib
import re
import struct
import sys
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
ART = ROOT / "installer" / "windows" / "art"
sys.path.insert(0, str(ART))
import make_art  # noqa: E402

# the insignia perennos.com shows; changing it is a brand decision, so the test pins it
INSIGNIA_SHA256 = "cc14e9668430f8786b2f3fcc2fd9c8a6ab42f342c266f8dcd8bb4d9d94af04b2"
CRATE_ICONS = {
    "perennos-tempo-launcher": "tempo-launch.ico",
    "perennos-tempo-local-agent": "tempo-local-agent.ico",
    "perennos-tempo-room": "tempo-room.ico",
    "perennos-tempo-studio": "tempo-studio.ico",
    "perennos-tempo-vault": "tempo-vault.ico",
}


def ico_frames(data):
    """[(size, bits, dib header size, dib width, dib height, rgba of the first pixel)] for every frame of an .ico."""
    reserved, kind, count = struct.unpack_from("<HHH", data)
    assert (reserved, kind) == (0, 1)
    frames = []
    for i in range(count):
        w, h, _, _, _, bits, length, offset = struct.unpack_from("<BBBBHHII", data, 6 + 16 * i)
        hdr, dw, dh = struct.unpack_from("<Iii", data, offset)
        b, g, r, a = data[offset + hdr: offset + hdr + 4]
        frames.append(((w or 256), bits, hdr, dw, dh, (r, g, b, a), offset, length))
    return frames


class Insignia(unittest.TestCase):
    def test_the_master_image_is_the_pinned_official_one(self):
        self.assertEqual(hashlib.sha256((ART / "perennos-insignia.png").read_bytes()).hexdigest(), INSIGNIA_SHA256)

    def test_the_icon_has_every_size_as_a_32_bit_bitmap(self):
        frames = ico_frames((ART / "perennos.ico").read_bytes())
        self.assertEqual([f[0] for f in frames], make_art.ICON_SIZES)
        for size, bits, hdr, dw, dh, _, _, _ in frames:
            self.assertEqual((bits, hdr, dw, dh), (32, 40, size, size * 2), size)   # a bitmap, which is what Inno Setup reads

    def test_the_corner_is_transparent_and_the_tile_is_the_brand_cream(self):
        data = (ART / "perennos.ico").read_bytes()
        big = [f for f in ico_frames(data) if f[0] == 256][0]
        offset, size = big[6], 256
        px = lambda x, y: tuple(data[offset + 40 + 4 * ((size - 1 - y) * size + x): offset + 40 + 4 * ((size - 1 - y) * size + x) + 4][i] for i in (2, 1, 0, 3))
        self.assertEqual(px(0, 0)[3], 0)                       # a rounded corner
        self.assertEqual(px(18, 128), (0xEC, 0xE6, 0xD8, 255))  # tile left of the P

    def test_every_program_has_the_same_official_icon(self):
        official = (ART / "perennos.ico").read_bytes()
        for crate, name in CRATE_ICONS.items():
            self.assertEqual((ROOT / crate / "assets" / name).read_bytes(), official, crate)

    def test_the_launcher_finds_its_icon_without_an_environment_variable(self):
        self.assertIn("assets/tempo-launch.ico", (ROOT / "perennos-tempo-launcher" / "build.rs").read_text(encoding="utf-8"))

    def test_the_installer_art_comes_out_as_the_official_icon_and_the_right_wizard_sizes(self):
        with tempfile.TemporaryDirectory() as out:
            make_art.main(out)
            for name in ("tempo.ico", "tempo-vault.ico", "tempo-room.ico"):
                self.assertEqual((pathlib.Path(out) / name).read_bytes(), (ART / "perennos.ico").read_bytes(), name)
            for pct, size in make_art.SMALL.items():
                bmp = (pathlib.Path(out) / f"wizard-small-{pct}.bmp").read_bytes()
                self.assertEqual(struct.unpack_from("<ii", bmp, 18), (size, size), pct)


class VersionDetails(unittest.TestCase):
    def test_each_program_says_perennos_and_names_its_part(self):
        for crate in CRATE_ICONS:
            text = (ROOT / crate / "build.rs").read_text(encoding="utf-8")
            self.assertIn('VALUE "CompanyName", "Perennos"', text, crate)
            self.assertIn('VALUE "ProductName", "Perennos Tempo"', text, crate)
            desc = re.search(r'VALUE "FileDescription", "([^"{]+)"', text) or re.search(r'DESCRIPTION: &str = "([^"]+)"', text)
            self.assertTrue(desc and desc.group(1).startswith("Perennos Tempo"), crate)

    def test_the_installer_and_uninstaller_use_the_icon_and_name_perennos(self):
        iss = (ROOT / "installer" / "windows" / "PerennosTempo.iss").read_text(encoding="utf-8")
        for line in ("SetupIconFile={#ArtDir}\\tempo.ico", "UninstallDisplayIcon={app}\\tempo.ico", "VersionInfoCompany={#Publisher}",
                     "VersionInfoProductName={#AppName}", "AppPublisher={#Publisher}"):
            self.assertIn(line, iss)
        self.assertRegex(iss, r'#define Publisher "Perennos"')


if __name__ == "__main__":
    unittest.main()
