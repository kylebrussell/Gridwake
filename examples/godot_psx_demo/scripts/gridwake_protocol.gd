class_name GridwakeProtocol
extends RefCounted

const PROTOCOL_MAGIC := "GW"
const PROTOCOL_VERSION := 2

const TAG_CLIENT_ACK_SNAPSHOT := 0x01
const TAG_CLIENT_INPUT := 0x02
const TAG_SERVER_SNAPSHOT_DELTA := 0x81
const TAG_SERVER_METRICS := 0x82

const OP_SPAWN_OR_ENTER := 0x01
const OP_UPDATE := 0x02
const OP_DESPAWN_OR_EXIT := 0x03

const CLIENT_INPUT_MAGIC := "GWCI"
const DEMO_PAYLOAD_MAGIC := "GWPD"

const KIND_BOT := 0
const KIND_PLAYER := 1
const KIND_EFFECT := 2
const KIND_COVER := 3

const LOD_FULL := 0
const LOD_REDUCED := 1
const LOD_MINIMAL := 2

static func encode_input(position: Vector3, yaw: float, fire: bool = false) -> PackedByteArray:
	var payload := StreamPeerBuffer.new()
	payload.big_endian = false
	payload.put_data(CLIENT_INPUT_MAGIC.to_ascii_buffer())
	payload.put_u8(2)
	payload.put_float(position.x)
	payload.put_float(position.y)
	payload.put_float(position.z)
	payload.put_float(yaw)
	var flags := 0
	if fire:
		flags |= 1
	payload.put_u8(flags)
	return _encode_client_payload(TAG_CLIENT_INPUT, payload.data_array)


static func encode_ack(sequence: int) -> PackedByteArray:
	var out := _message_writer(TAG_CLIENT_ACK_SNAPSHOT)
	out.put_u64(sequence)
	return out.data_array


static func decode_server_packet(packet: PackedByteArray) -> Dictionary:
	var stream := _reader(packet)
	if stream.get_available_bytes() < 4:
		return {"type": "invalid", "reason": "short header"}

	var magic: String = stream.get_data(2)[1].get_string_from_ascii()
	if magic != PROTOCOL_MAGIC:
		return {"type": "invalid", "reason": "bad magic"}

	var version := stream.get_u8()
	if version != PROTOCOL_VERSION:
		return {"type": "invalid", "reason": "bad version", "version": version}

	var tag := stream.get_u8()
	match tag:
		TAG_SERVER_SNAPSHOT_DELTA:
			return _decode_snapshot(stream)
		TAG_SERVER_METRICS:
			return _decode_metrics(stream)
		_:
			return {"type": "invalid", "reason": "unknown tag", "tag": tag}


static func decode_demo_payload(payload: PackedByteArray) -> Dictionary:
	var stream := _reader(payload)
	if stream.get_available_bytes() < 24:
		return {}
	if stream.get_data(4)[1].get_string_from_ascii() != DEMO_PAYLOAD_MAGIC:
		return {}

	var version := stream.get_u8()
	if version != 1:
		return {}

	var kind := stream.get_u8()
	var team := stream.get_u8()
	var lod := stream.get_u8()
	var position := Vector3(stream.get_float(), stream.get_float(), stream.get_float())
	var yaw := stream.get_float()
	var radius := 0.85
	var phase := 0.0
	var style := 0

	if lod == LOD_FULL or lod == LOD_REDUCED:
		radius = stream.get_float()
	if lod == LOD_FULL:
		phase = stream.get_float()
		style = int(stream.get_u32())

	return {
		"kind": kind,
		"team": team,
		"lod": lod,
		"position": position,
		"yaw": yaw,
		"radius": radius,
		"phase": phase,
		"style": style,
	}


static func _encode_client_payload(tag: int, payload: PackedByteArray) -> PackedByteArray:
	var out := _message_writer(tag)
	out.put_u32(payload.size())
	out.put_data(payload)
	return out.data_array


static func _message_writer(tag: int) -> StreamPeerBuffer:
	var out := StreamPeerBuffer.new()
	out.big_endian = false
	out.put_data(PROTOCOL_MAGIC.to_ascii_buffer())
	out.put_u8(PROTOCOL_VERSION)
	out.put_u8(tag)
	return out


static func _decode_snapshot(stream: StreamPeerBuffer) -> Dictionary:
	var sequence := int(stream.get_u64())
	var baseline := -1
	var has_baseline := stream.get_u8()
	if has_baseline == 1:
		baseline = int(stream.get_u64())

	var op_count := int(stream.get_u32())
	var ops: Array[Dictionary] = []
	for _index in op_count:
		var op_tag := stream.get_u8()
		var entity := int(stream.get_u64())
		match op_tag:
			OP_SPAWN_OR_ENTER, OP_UPDATE:
				var payload_len := int(stream.get_u32())
				var payload: PackedByteArray = stream.get_data(payload_len)[1]
				ops.append({
					"op": op_tag,
					"entity": entity,
					"payload": payload,
				})
			OP_DESPAWN_OR_EXIT:
				ops.append({
					"op": op_tag,
					"entity": entity,
				})
			_:
				return {"type": "invalid", "reason": "unknown op", "op": op_tag}

	return {
		"type": "snapshot",
		"sequence": sequence,
		"baseline": baseline,
		"ops": ops,
	}


static func _decode_metrics(stream: StreamPeerBuffer) -> Dictionary:
	return {
		"type": "metrics",
		"tick": int(stream.get_u64()),
		"clients": int(stream.get_u64()),
		"entities": int(stream.get_u64()),
		"aoi_candidates": int(stream.get_u64()),
		"selected_updates": int(stream.get_u64()),
		"selected_full_lod_updates": int(stream.get_u64()),
		"selected_reduced_lod_updates": int(stream.get_u64()),
		"selected_minimal_lod_updates": int(stream.get_u64()),
		"deferred_updates": int(stream.get_u64()),
		"bytes_scheduled": int(stream.get_u64()),
		"deferred_bytes": int(stream.get_u64()),
	}


static func _reader(bytes: PackedByteArray) -> StreamPeerBuffer:
	var stream := StreamPeerBuffer.new()
	stream.big_endian = false
	stream.data_array = bytes
	stream.seek(0)
	return stream
