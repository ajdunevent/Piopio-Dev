"""Cross-checks piopio-omemo against python-oldmemo, in both directions.

python-oldmemo (AGPL-3.0) is only run here as a test oracle; none of its code
is part of Piopio. Usage:

    pip install oldmemo omemo
    cargo build -p piopio-omemo --example oracle_cli
    python3 tests/oracle/check.py target/debug/examples/oracle_cli
"""
import asyncio, base64, json, os, subprocess, sys, tempfile
import xml.etree.ElementTree as ET

from omemo.storage import Storage, Just, Nothing
import oldmemo.etree as oet
from oldmemo.oldmemo import (Oldmemo, KeyExchangeImpl, EncryptedKeyMaterialImpl,
                             ContentImpl)

CLI = sys.argv[1]
TMP = tempfile.mkdtemp()
NS = "eu.siacs.conversations.axolotl"


def rust(*args):
    out = subprocess.run([CLI, *args], check=True, capture_output=True, text=True).stdout.strip()
    return out


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


async def rust_starts():
    """Rust initiates with Python's bundle; both sides exchange messages."""
    py = Oldmemo(Memory())
    await py.generate_pre_keys(100)
    bundle = oet.serialize_bundle(await py.get_bundle("bob@example.org", 222))
    spk = bundle.find(f"{{{NS}}}signedPreKeyPublic")
    rust_bundle = {
        "identity": base64.b64decode(bundle.find(f"{{{NS}}}identityKey").text).hex(),
        "spk_id": int(spk.get("signedPreKeyId")),
        "spk": base64.b64decode(spk.text).hex(),
        "sig": base64.b64decode(bundle.find(f"{{{NS}}}signedPreKeySignature").text).hex(),
        "prekeys": [[int(p.get("preKeyId")), base64.b64decode(p.text).hex()]
                    for p in bundle.find(f"{{{NS}}}prekeys")],
    }
    with open(path("bob-bundle.json"), "w") as f:
        json.dump(rust_bundle, f)
    rust("gen", "111", path("alice-keys.json"))
    rust("initiate", path("alice-keys.json"), path("bob-bundle.json"), path("alice-session.json"))

    for i, text in enumerate(["hello from Rust", "still no answer"]):
        payload = json.loads(rust("payload-encrypt", text))
        enc = json.loads(rust("encrypt", path("alice-session.json"), payload["key"]))
        assert enc["prekey"], "key exchange until answered"
        kex, authenticated = KeyExchangeImpl.parse(bytes.fromhex(enc["data"]), False)
        ekm = EncryptedKeyMaterialImpl.parse(authenticated, "alice@example.org", 111)
        if i == 0:
            session, pkm = await py.build_session_passive("alice@example.org", 111, kex, ekm)
            await py.store_session(session)
        else:
            session = await py.load_session("alice@example.org", 111)
            assert session.key_exchange.builds_same_session(kex)
            pkm = await py.decrypt_key_material(session, ekm)
            await py.store_session(session)
        content = ContentImpl(bytes.fromhex(payload["ct"]), bytes.fromhex(payload["iv"]))
        assert (await py.decrypt_plaintext(content, pkm)).decode() == text

    for text in ["hello from Python", "and again"]:
        content, pkm = await py.encrypt_plaintext(text.encode())
        ekm = await py.encrypt_key_material(session, pkm)
        await py.store_session(session)
        key = rust("decrypt", path("alice-session.json"), ekm.serialize().hex())
        assert rust("payload-decrypt", key, content.initialization_vector.hex(),
                    content.ciphertext.hex()) == text

    payload = json.loads(rust("payload-encrypt", "after the answer"))
    enc = json.loads(rust("encrypt", path("alice-session.json"), payload["key"]))
    assert not enc["prekey"], "no key exchange once answered"
    ekm = EncryptedKeyMaterialImpl.parse(bytes.fromhex(enc["data"]), "alice@example.org", 111)
    pkm = await py.decrypt_key_material(session, ekm)
    content = ContentImpl(bytes.fromhex(payload["ct"]), bytes.fromhex(payload["iv"]))
    assert (await py.decrypt_plaintext(content, pkm)).decode() == "after the answer"
    print("Rust → Python: ok")


async def python_starts():
    """Python initiates with Rust's bundle; both sides exchange messages."""
    py = Oldmemo(Memory())
    b = json.loads(rust("gen", "444", path("bob2-keys.json")))
    xml = ET.Element(f"{{{NS}}}bundle")
    spk = ET.SubElement(xml, f"{{{NS}}}signedPreKeyPublic", signedPreKeyId=str(b["spk_id"]))
    spk.text = b64(bytes.fromhex(b["spk"]))
    ET.SubElement(xml, f"{{{NS}}}signedPreKeySignature").text = b64(bytes.fromhex(b["sig"]))
    ET.SubElement(xml, f"{{{NS}}}identityKey").text = b64(bytes.fromhex(b["identity"]))
    pks = ET.SubElement(xml, f"{{{NS}}}prekeys")
    for pid, pk in b["prekeys"]:
        ET.SubElement(pks, f"{{{NS}}}preKeyPublic", preKeyId=str(pid)).text = b64(bytes.fromhex(pk))
    bundle = oet.parse_bundle(xml, "bob2@example.org", 444)

    content, pkm = await py.encrypt_plaintext(b"Python starts")
    session, ekm = await py.build_session_active("bob2@example.org", 444, bundle, pkm)
    await py.store_session(session)
    data, _ = session.key_exchange.serialize(ekm.serialize())
    key = rust("accept", path("bob2-keys.json"), data.hex(), path("bob2-session.json"))
    assert rust("payload-decrypt", key, content.initialization_vector.hex(),
                content.ciphertext.hex()) == "Python starts"

    for text in ["Rust answers", "twice"]:
        payload = json.loads(rust("payload-encrypt", text))
        enc = json.loads(rust("encrypt", path("bob2-session.json"), payload["key"]))
        assert not enc["prekey"]
        ekm = EncryptedKeyMaterialImpl.parse(bytes.fromhex(enc["data"]), "bob2@example.org", 444)
        pkm = await py.decrypt_key_material(session, ekm)
        await py.store_session(session)
        content = ContentImpl(bytes.fromhex(payload["ct"]), bytes.fromhex(payload["iv"]))
        assert (await py.decrypt_plaintext(content, pkm)).decode() == text

    content, pkm = await py.encrypt_plaintext(b"ratchet turned")
    ekm = await py.encrypt_key_material(session, pkm)
    key = rust("decrypt", path("bob2-session.json"), ekm.serialize().hex())
    assert rust("payload-decrypt", key, content.initialization_vector.hex(),
                content.ciphertext.hex()) == "ratchet turned"
    print("Python → Rust: ok")


asyncio.run(rust_starts())
asyncio.run(python_starts())
