#!/usr/bin/env python3
"""Generate synthetic DICOM test data and upload it to Orthanc.

Creates two things worth testing differently:
  - a multi-frame ultrasound cine (time axis, native frame rate in the tags)
  - a 40-slice CT stack in Hounsfield units (spatial axis, so the soft/lung/
    bone window presets produce visibly different renditions)

Usage:
    python3 scripts/make_test_study.py [--orthanc http://localhost:8042]
"""
import argparse
import base64
import datetime
import io
import json
import math
import urllib.request

import numpy as np
import pydicom
from pydicom.dataset import FileDataset, FileMetaDataset
from pydicom.uid import ExplicitVRLittleEndian, generate_uid

STUDY_UID = generate_uid()
NOW = datetime.datetime.now()


def base_dataset(sop_class: str, sop_uid: str, series_uid: str, modality: str) -> FileDataset:
    meta = FileMetaDataset()
    meta.MediaStorageSOPClassUID = sop_class
    meta.MediaStorageSOPInstanceUID = sop_uid
    meta.TransferSyntaxUID = ExplicitVRLittleEndian

    ds = FileDataset(None, {}, file_meta=meta, preamble=b"\x00" * 128)
    ds.SOPClassUID = sop_class
    ds.SOPInstanceUID = sop_uid
    ds.StudyInstanceUID = STUDY_UID
    ds.SeriesInstanceUID = series_uid
    ds.PatientName = "OMV^TestPhantom"
    ds.PatientID = "OMV-TEST-001"
    ds.StudyDate = NOW.strftime("%Y%m%d")
    ds.StudyTime = NOW.strftime("%H%M%S")
    ds.StudyDescription = "OMV synthetic test study"
    ds.Modality = modality
    ds.is_little_endian = True
    ds.is_implicit_VR = False
    return ds


def us_cine(frames=60, size=256) -> bytes:
    """Multi-frame US: a bright arc sweeping like a beating structure."""
    series_uid = generate_uid()
    sop_uid = generate_uid()
    ds = base_dataset("1.2.840.10008.5.1.4.1.1.3.1", sop_uid, series_uid, "US")
    ds.SeriesDescription = "Synthetic US cine"
    ds.SeriesNumber = 1
    ds.InstanceNumber = 1
    ds.NumberOfFrames = frames
    ds.RecommendedDisplayFrameRate = 20
    ds.FrameTime = 50.0  # ms
    ds.Rows = ds.Columns = size
    ds.SamplesPerPixel = 1
    ds.PhotometricInterpretation = "MONOCHROME2"
    ds.BitsAllocated = ds.BitsStored = 8
    ds.HighBit = 7
    ds.PixelRepresentation = 0

    yy, xx = np.mgrid[0:size, 0:size].astype(np.float32)
    cx = cy = size / 2
    r = np.hypot(xx - cx, yy - cy)
    stack = []
    for f in range(frames):
        phase = 2 * math.pi * f / frames
        radius = size * (0.25 + 0.08 * math.sin(phase))
        ring = np.exp(-((r - radius) ** 2) / 60.0) * 255
        speckle = np.random.default_rng(f).uniform(0, 40, (size, size))
        stack.append(np.clip(ring + speckle, 0, 255).astype(np.uint8))
    ds.PixelData = np.stack(stack).tobytes()
    return dcm_bytes(ds)


def ct_slice(z: int, n_slices: int, size=256) -> bytes:
    """One CT slice in HU: air background, water cylinder, bone ring, a lung-
    density pocket — so each window preset shows different anatomy."""
    series_uid = ct_slice.series_uid
    sop_uid = generate_uid()
    ds = base_dataset("1.2.840.10008.5.1.4.1.1.2", sop_uid, series_uid, "CT")
    ds.SeriesDescription = "Synthetic CT stack"
    ds.SeriesNumber = 2
    ds.InstanceNumber = z + 1
    ds.ImagePositionPatient = [0, 0, float(z) * 2.5]
    ds.ImageOrientationPatient = [1, 0, 0, 0, 1, 0]
    ds.SliceThickness = 2.5
    ds.Rows = ds.Columns = size
    ds.SamplesPerPixel = 1
    ds.PhotometricInterpretation = "MONOCHROME2"
    ds.BitsAllocated = ds.BitsStored = 16
    ds.HighBit = 15
    ds.PixelRepresentation = 0
    ds.RescaleIntercept = -1024.0
    ds.RescaleSlope = 1.0
    ds.WindowCenter = 40
    ds.WindowWidth = 400

    yy, xx = np.mgrid[0:size, 0:size].astype(np.float32)
    cx = cy = size / 2
    r = np.hypot(xx - cx, yy - cy)
    hu = np.full((size, size), -1000.0, dtype=np.float32)          # air
    body = r < size * 0.4
    hu[body] = 40.0                                                 # soft tissue
    ring = (r > size * 0.36) & (r < size * 0.4)
    hu[ring] = 700.0                                                # bone shell
    # a "lung" pocket whose size varies along z
    pocket_r = size * 0.12 * (0.5 + z / n_slices)
    pocket = np.hypot(xx - cx * 0.7, yy - cy) < pocket_r
    hu[pocket & body] = -750.0
    stored = np.clip(hu + 1024.0, 0, 4095).astype(np.uint16)
    ds.PixelData = stored.tobytes()
    return dcm_bytes(ds)


ct_slice.series_uid = generate_uid()


def dcm_bytes(ds: FileDataset) -> bytes:
    buf = io.BytesIO()
    try:
        pydicom.dcmwrite(buf, ds, enforce_file_format=True)  # pydicom >= 3
    except TypeError:
        pydicom.dcmwrite(buf, ds, write_like_original=False)  # pydicom 2.x

    return buf.getvalue()


def upload(orthanc: str, blob: bytes) -> None:
    req = urllib.request.Request(f"{orthanc}/instances", data=blob, method="POST")
    req.add_header("Content-Type", "application/dicom")
    req.add_header(
        "Authorization", "Basic " + base64.b64encode(b"omv:omv").decode()
    )
    with urllib.request.urlopen(req) as res:
        json.load(res)


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--orthanc", default="http://localhost:8042")
    ap.add_argument("--slices", type=int, default=40)
    args = ap.parse_args()

    print(f"study UID: {STUDY_UID}")
    upload(args.orthanc, us_cine())
    print("uploaded: US cine (60 frames @ 20 fps)")
    for z in range(args.slices):
        upload(args.orthanc, ct_slice(z, args.slices))
    print(f"uploaded: CT stack ({args.slices} slices)")
    print("waiting for Orthanc StableAge; conversion starts automatically.")
