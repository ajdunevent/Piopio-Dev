"""Cross-checks piopio-omemo's OMEMO 2 against python-twomemo (MIT), in
both directions. Usage:

    pip install "twomemo[xml]" omemo
    cargo build -p piopio-omemo --example oracle_cli
    python3 tests/oracle/check_v2.py target/debug/examples/oracle_cli
"""
import asyncio, base64, json, os, subprocess, sys, tempfile
import xml.etree.ElementTree as ET

from omemo.storage import Storage, Just, Nothing
import twomemo.etree as tet
from twomemo.twomemo import (Twomemo, KeyExchangeImpl, EncryptedKeyMaterialImpl,
                             ContentImpl, PlainKeyMaterialImpl)

CLI = sys.argv[1]
TMP = tempfile.mkdtemp()
NS = "urn:xmpp:omemo:2"


def rust(*args):
    return subprocess.run([CLI, *args], check=True, capture_output=True, text=True).stdout.strip()


def path(name):
    return os.path.join(TMP, name)


class Memory(Storage):
    def __init__(self):
        super().__init__(True)
        self.data = {}

    async def _load(self, key):
        return Just(self.data[key]) if key in self.data else Nothing()

    async def _store(self, key, value):
        self.data[key] = value

    async def _delete(self, key):
        self.data.pop(key, None)


def b64(b):
    return base64.b64encode(b).decode()


def unb64(s):
    return base64.b64decode(s)


async def rust_starts():
    py = Twomemo(Memory())
    await py.generate_pre_keys(100)
    bundle = tet.serialize_bundle(await py.get_bundle("bob@example.org", 222))
    spk = bundle.find(f"{{{NS}}}spk")
    with open(path("bob-bundle.json"), "w") as f:
        json.dump({
            "identity": unb64(bundle.find(f"{{{NS}}}ik").text).hex(),
            "spk_id": int(spk.get("id")),
            "spk": unb64(spk.text).hex(),
            "sig": unb64(bundle.find(f"{{{NS}}}spks").text).hex(),
            "prekeys": [[int(p.get("id")), unb64(p.text).hex()]
                        for p in bundle.find(f"{{{NS}}}prekeys")],
        }, f)
    rust("v2-gen", "111", path("alice-keys.json"))
    rust("v2-initiate", path("alice-keys.json"), path("bob-bundle.json"), path("alice-session.json"))

    session = None
    for i, text in enumerate(["hello from Rust", "still no answer"]):
        payload = json.loads(rust("v2-payload-encrypt", text))
        enc = json.loads(rust("v2-encrypt", path("alice-session.json"), payload["key"]))
        assert enc["kex"]
        kex, authenticated = KeyExchangeImpl.parse(bytes.fromhex(enc["data"]))
        ekm = EncryptedKeyMaterialImpl.parse(authenticated, "alice@example.org", 111)
        if i == 0:
            session, pkm = await py.build_session_passive("alice@example.org", 111, kex, ekm)
        else:
            assert session.key_exchange.builds_same_session(kex)
            pkm = await py.decrypt_key_material(session, ekm)
        await py.store_session(session)
        content = ContentImpl(bytes.fromhex(payload["ct"]))
        assert (await py.decrypt_plaintext(content, pkm)).decode() == text

    for text in ["hello from Python", "and again"]:
        content, pkm = await py.encrypt_plaintext(text.encode())
        ekm = await py.encrypt_key_material(session, pkm)
        await py.store_session(session)
        km = rust("v2-decrypt", path("alice-session.json"), ekm.serialize().hex())
        assert rust("v2-payload-decrypt", km, content.ciphertext.hex()) == text

    payload = json.loads(rust("v2-payload-encrypt", "after the answer"))
    enc = json.loads(rust("v2-encrypt", path("alice-session.json"), payload["key"]))
    assert not enc["kex"]
    ekm = EncryptedKeyMaterialImpl.parse(bytes.fromhex(enc["data"]), "alice@example.org", 111)
    pkm = await py.decrypt_key_material(session, ekm)
    assert (await py.decrypt_plaintext(ContentImpl(bytes.fromhex(payload["ct"])), pkm)).decode() == "after the answer"
    print("OMEMO 2, Rust → Python: ok")


async def python_starts():
    py = Twomemo(Memory())
    b = json.loads(rust("v2-gen", "444", path("bob2-keys.json")))
    xml = ET.Element(f"{{{NS}}}bundle")
    ET.SubElement(xml, f"{{{NS}}}spk", id=str(b["spk_id"])).text = b64(bytes.fromhex(b["spk"]))
    ET.SubElement(xml, f"{{{NS}}}spks").text = b64(bytes.fromhex(b["sig"]))
    ET.SubElement(xml, f"{{{NS}}}ik").text = b64(bytes.fromhex(b["identity"]))
    pks = ET.SubElement(xml, f"{{{NS}}}prekeys")
    for pid, pk in b["prekeys"]:
        ET.SubElement(pks, f"{{{NS}}}pk", id=str(pid)).text = b64(bytes.fromhex(pk))
    bundle = tet.parse_bundle(xml, "bob2@example.org", 444)

    content, pkm = await py.encrypt_plaintext(b"Python starts")
    session, ekm = await py.build_session_active("bob2@example.org", 444, bundle, pkm)
    await py.store_session(session)
    data = session.key_exchange.serialize(ekm.serialize())
    km = rust("v2-accept", path("bob2-keys.json"), data.hex(), path("bob2-session.json"))
    assert rust("v2-payload-decrypt", km, content.ciphertext.hex()) == "Python starts"

    for text in ["Rust answers", "twice"]:
        payload = json.loads(rust("v2-payload-encrypt", text))
        enc = json.loads(rust("v2-encrypt", path("bob2-session.json"), payload["key"]))
        assert not enc["kex"]
        ekm = EncryptedKeyMaterialImpl.parse(bytes.fromhex(enc["data"]), "bob2@example.org", 444)
        pkm = await py.decrypt_key_material(session, ekm)
        await py.store_session(session)
        assert (await py.decrypt_plaintext(ContentImpl(bytes.fromhex(payload["ct"])), pkm)).decode() == text

    content, pkm = await py.encrypt_plaintext(b"ratchet turned")
    ekm = await py.encrypt_key_material(session, pkm)
    km = rust("v2-decrypt", path("bob2-session.json"), ekm.serialize().hex())
    assert rust("v2-payload-decrypt", km, content.ciphertext.hex()) == "ratchet turned"
    print("OMEMO 2, Python → Rust: ok")


asyncio.run(rust_starts())
asyncio.run(python_starts())
