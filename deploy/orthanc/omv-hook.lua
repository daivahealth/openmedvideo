-- Fires once a study is stable (all instances received). Enqueues a
-- conversion job via the API; the API is idempotent on re-delivery.
function OnStableStudy(studyId, tags, metadata)
  local body = '{"study_id": "' .. studyId .. '"}'
  local headers = { ["content-type"] = "application/json" }
  local ok, err = pcall(function()
    HttpPost("http://api:8080/internal/orthanc-event", body, headers)
  end)
  if ok then
    print("OMV: enqueued conversion for study " .. studyId)
  else
    print("OMV: failed to enqueue study " .. studyId .. ": " .. tostring(err))
  end
end
