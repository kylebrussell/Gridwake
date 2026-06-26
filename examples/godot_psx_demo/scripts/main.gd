extends Node3D

const GridwakeProtocol := preload("res://scripts/gridwake_protocol.gd")

@export var server_host := "127.0.0.1"
@export var server_port := 3456
@export var input_send_hz := 30.0
@export var move_speed := 28.0
@export var turn_speed := 2.4
@export var mouse_sensitivity := 0.0032
@export var camera_follow_distance := 9.0
@export var camera_height := 4.2
@export var camera_smoothing := 18.0
@export var server_position_snap_distance := 28.0
@export var max_visual_entities := 6000
@export var max_packets_per_frame := 32
@export var max_pending_fragment_sequences := 8
@export var perf_sample_hz := 4.0
@export var perf_log_hz := 1.0
@export var perf_log_enabled := true
@export var show_perf_overlay := false

var udp := PacketPeerUDP.new()
var entity_nodes: Dictionary = {}
var entity_material_keys: Dictionary = {}
var instanced_entity_slots: Dictionary = {}
var instanced_buckets: Dictionary = {}
var player_position := Vector3(0.0, 0.5, 18.0)
var player_yaw := 0.0
var camera_initialized := false
var camera_look_position := Vector3.ZERO
var input_accumulator := 0.0
var snapshot_sequence := -1
var packets_received := 0
var packets_backlogged := 0
var udp_packets_received := 0
var udp_packets_last_frame := 0
var ops_received := 0
var fragments_received := 0
var fragments_completed := 0
var last_server_error := ""
var snapshot_fragments: Dictionary = {}
var fragment_sequence_order: Array[String] = []
var perf_sample_accumulator := 0.0
var perf_log_accumulator := 0.0
var perf_last_sample_msec := 0
var perf_last_udp_packets := 0
var perf_last_snapshots := 0
var perf_last_ops := 0
var perf_last_fragments := 0
var perf_fps := 0.0
var perf_frame_ms := 0.0
var perf_process_ms := 0.0
var perf_physics_ms := 0.0
var perf_draw_calls := 0
var perf_render_objects := 0
var perf_render_primitives := 0
var perf_node_count := 0
var perf_resource_count := 0
var perf_static_memory_mb := 0.0
var perf_udp_packets_per_sec := 0.0
var perf_snapshots_per_sec := 0.0
var perf_ops_per_sec := 0.0
var perf_fragments_per_sec := 0.0

var bot_mesh: BoxMesh
var player_mesh: BoxMesh
var effect_mesh: SphereMesh
var impact_mesh: SphereMesh
var cover_mesh: BoxMesh
var prop_mesh: BoxMesh
var materials: Dictionary = {}
var camera: Camera3D
var hud: Label
var crosshair_root: Control
var cover_nodes: Dictionary = {}
var damaged_cover_nodes: Dictionary = {}
var ruined_cover_nodes: Dictionary = {}
var impact_nodes: Dictionary = {}
var score_nodes: Dictionary = {}
var local_player_entity := -1
var local_player_team := -1
var local_player_health := 1.0
var has_local_server_position := false
var local_server_position := Vector3.ZERO
var red_score := 0
var blue_score := 0
var winner_team := -1
var round_index := 1

func _ready() -> void:
	Input.set_mouse_mode(Input.MOUSE_MODE_CAPTURED)
	_setup_rendering()
	_setup_world()
	_setup_udp()
	perf_last_sample_msec = Time.get_ticks_msec()
	_sample_perf()
	_send_input()


func _exit_tree() -> void:
	udp.close()


func _input(event: InputEvent) -> void:
	if event is InputEventMouseButton and event.pressed:
		Input.set_mouse_mode(Input.MOUSE_MODE_CAPTURED)
	if event is InputEventMouseMotion and Input.get_mouse_mode() == Input.MOUSE_MODE_CAPTURED and local_player_health > 0.0:
		player_yaw -= event.relative.x * mouse_sensitivity
	if event is InputEventKey and event.pressed and event.keycode == KEY_ESCAPE:
		if Input.get_mouse_mode() == Input.MOUSE_MODE_CAPTURED:
			Input.set_mouse_mode(Input.MOUSE_MODE_VISIBLE)
		else:
			Input.set_mouse_mode(Input.MOUSE_MODE_CAPTURED)


func _process(delta: float) -> void:
	_update_camera(delta)
	_update_hud()


func _physics_process(delta: float) -> void:
	_update_player(delta)
	_poll_network()
	_update_perf(delta)
	input_accumulator += delta
	if input_accumulator >= 1.0 / input_send_hz:
		input_accumulator = 0.0
		_send_input()


func _setup_rendering() -> void:
	var world := WorldEnvironment.new()
	var environment := Environment.new()
	environment.background_mode = Environment.BG_COLOR
	environment.background_color = Color(0.075, 0.082, 0.105)
	environment.fog_enabled = true
	environment.fog_density = 0.018
	environment.fog_light_color = Color(0.36, 0.39, 0.48)
	world.environment = environment
	add_child(world)

	var sun := DirectionalLight3D.new()
	sun.rotation_degrees = Vector3(-55.0, 35.0, 0.0)
	sun.light_energy = 1.4
	add_child(sun)

	camera = Camera3D.new()
	camera.fov = 58.0
	add_child(camera)


func _setup_world() -> void:
	bot_mesh = BoxMesh.new()
	bot_mesh.size = Vector3(0.9, 1.3, 0.9)
	player_mesh = BoxMesh.new()
	player_mesh.size = Vector3(1.1, 1.7, 1.1)
	effect_mesh = SphereMesh.new()
	effect_mesh.radial_segments = 8
	effect_mesh.rings = 4
	effect_mesh.radius = 0.65
	effect_mesh.height = 1.3
	impact_mesh = SphereMesh.new()
	impact_mesh.radial_segments = 8
	impact_mesh.rings = 4
	impact_mesh.radius = 1.0
	impact_mesh.height = 2.0
	cover_mesh = BoxMesh.new()
	cover_mesh.size = Vector3.ONE
	prop_mesh = BoxMesh.new()
	prop_mesh.size = Vector3.ONE

	_create_materials()
	_create_floor()
	_create_arena_props()

	var canvas := CanvasLayer.new()
	hud = Label.new()
	hud.position = Vector2(16.0, 16.0)
	hud.add_theme_color_override("font_color", Color(0.78, 0.95, 1.0))
	hud.add_theme_color_override("font_shadow_color", Color.BLACK)
	hud.add_theme_constant_override("shadow_offset_x", 2)
	hud.add_theme_constant_override("shadow_offset_y", 2)
	canvas.add_child(hud)
	_create_crosshair(canvas)
	add_child(canvas)


func _create_crosshair(canvas: CanvasLayer) -> void:
	crosshair_root = Control.new()
	crosshair_root.set_anchors_preset(Control.PRESET_FULL_RECT)
	crosshair_root.mouse_filter = Control.MOUSE_FILTER_IGNORE
	canvas.add_child(crosshair_root)

	var color := Color(0.78, 0.95, 1.0, 0.78)
	crosshair_root.add_child(_crosshair_rect(Vector2(-1.0, -1.0), Vector2(2.0, 2.0), color))
	crosshair_root.add_child(_crosshair_rect(Vector2(-1.0, -14.0), Vector2(2.0, 8.0), color))
	crosshair_root.add_child(_crosshair_rect(Vector2(-1.0, 6.0), Vector2(2.0, 8.0), color))
	crosshair_root.add_child(_crosshair_rect(Vector2(-14.0, -1.0), Vector2(8.0, 2.0), color))
	crosshair_root.add_child(_crosshair_rect(Vector2(6.0, -1.0), Vector2(8.0, 2.0), color))


func _crosshair_rect(offset: Vector2, size: Vector2, color: Color) -> ColorRect:
	var rect := ColorRect.new()
	rect.color = color
	rect.anchor_left = 0.5
	rect.anchor_top = 0.5
	rect.anchor_right = 0.5
	rect.anchor_bottom = 0.5
	rect.offset_left = offset.x
	rect.offset_top = offset.y
	rect.offset_right = offset.x + size.x
	rect.offset_bottom = offset.y + size.y
	rect.mouse_filter = Control.MOUSE_FILTER_IGNORE
	return rect


func _create_materials() -> void:
	materials["bot_red"] = _material(Color(0.95, 0.18, 0.22))
	materials["bot_red_hurt"] = _material(Color(0.58, 0.10, 0.12))
	materials["bot_blue"] = _material(Color(0.18, 0.50, 1.0))
	materials["bot_blue_hurt"] = _material(Color(0.10, 0.23, 0.58))
	materials["player_red"] = _material(Color(1.0, 0.46, 0.42))
	materials["player_blue"] = _material(Color(0.42, 0.72, 1.0))
	materials["player_down"] = _material(Color(0.16, 0.16, 0.18))
	materials["effect_red"] = _material(Color(1.0, 0.35, 0.42))
	materials["effect_blue"] = _material(Color(0.30, 0.62, 1.0))
	materials["impact"] = _material(Color(0.82, 0.36, 0.20))
	materials["score_red"] = _material(Color(1.0, 0.10, 0.08))
	materials["score_blue"] = _material(Color(0.10, 0.42, 1.0))
	materials["minimal"] = _material(Color(0.52, 0.57, 0.66))
	materials["cover_0"] = _material(Color(0.43, 0.48, 0.50))
	materials["cover_1"] = _material(Color(0.50, 0.41, 0.34))
	materials["cover_2"] = _material(Color(0.34, 0.42, 0.34))
	materials["cover_3"] = _material(Color(0.45, 0.37, 0.48))
	materials["cover_damaged"] = _material(Color(0.72, 0.48, 0.28))
	materials["cover_ruined"] = _material(Color(0.16, 0.15, 0.15))
	materials["floor"] = _material(Color(0.13, 0.16, 0.18))
	materials["grid"] = _material(Color(0.22, 0.28, 0.32))
	materials["prop_wall"] = _material(Color(0.38, 0.43, 0.48))
	materials["prop_trim"] = _material(Color(0.14, 0.52, 0.56))
	materials["prop_signal"] = _material(Color(0.35, 0.70, 0.66))
	materials["prop_barrel"] = _material(Color(0.62, 0.18, 0.20))


func _material(color: Color) -> StandardMaterial3D:
	var material := StandardMaterial3D.new()
	material.albedo_color = color
	material.shading_mode = BaseMaterial3D.SHADING_MODE_UNSHADED
	material.texture_filter = BaseMaterial3D.TEXTURE_FILTER_NEAREST
	return material


func _create_floor() -> void:
	var floor_mesh := BoxMesh.new()
	floor_mesh.size = Vector3(560.0, 0.15, 560.0)
	var floor := MeshInstance3D.new()
	floor.mesh = floor_mesh
	floor.material_override = materials["floor"]
	floor.position.y = -0.1
	add_child(floor)

	var line_mesh := BoxMesh.new()
	line_mesh.size = Vector3(560.0, 0.08, 0.18)
	for index in range(-8, 9):
		var z_line := MeshInstance3D.new()
		z_line.mesh = line_mesh
		z_line.material_override = materials["grid"]
		z_line.position = Vector3(0.0, 0.02, float(index) * 32.0)
		add_child(z_line)

		var x_line := MeshInstance3D.new()
		x_line.mesh = line_mesh
		x_line.material_override = materials["grid"]
		x_line.rotation_degrees.y = 90.0
		x_line.position = Vector3(float(index) * 32.0, 0.03, 0.0)
		add_child(x_line)


func _create_arena_props() -> void:
	var walls: Array[Transform3D] = []
	var trim: Array[Transform3D] = []
	var signal_props: Array[Transform3D] = []
	var barrels: Array[Transform3D] = []

	for lane in range(-4, 5):
		var x := float(lane) * 18.0
		walls.append(_box_transform(Vector3(x, 1.4, -74.0), Vector3(10.0, 2.8, 2.2), 0.0))
		walls.append(_box_transform(Vector3(x, 1.4, 74.0), Vector3(10.0, 2.8, 2.2), 0.0))
		if lane % 2 == 0:
			trim.append(_box_transform(Vector3(-82.0, 2.8, x), Vector3(2.0, 5.6, 8.0), 0.0))
			trim.append(_box_transform(Vector3(82.0, 2.8, x), Vector3(2.0, 5.6, 8.0), 0.0))

	for index in range(18):
		var angle := float(index) * TAU / 18.0
		var radius := 34.0 + float(index % 3) * 15.0
		var position := Vector3(cos(angle) * radius, 0.85, sin(angle) * radius)
		barrels.append(_box_transform(position, Vector3(2.1, 1.7, 2.1), angle))

	for corner_x in [-54.0, 54.0]:
		for corner_z in [-54.0, 54.0]:
			signal_props.append(_box_transform(Vector3(corner_x, 4.2, corner_z), Vector3(4.0, 8.4, 4.0), 0.0))
			trim.append(_box_transform(Vector3(corner_x, 8.8, corner_z), Vector3(6.0, 0.8, 6.0), 0.0))

	_create_static_prop_bucket("prop_wall", walls)
	_create_static_prop_bucket("prop_trim", trim)
	_create_static_prop_bucket("prop_signal", signal_props)
	_create_static_prop_bucket("prop_barrel", barrels)


func _box_transform(position: Vector3, size: Vector3, yaw: float) -> Transform3D:
	var basis := Basis(Vector3.UP, yaw)
	basis = basis.scaled(size)
	return Transform3D(basis, position)


func _create_static_prop_bucket(material_key: String, transforms: Array[Transform3D]) -> void:
	if transforms.is_empty():
		return
	var multimesh := MultiMesh.new()
	multimesh.transform_format = MultiMesh.TRANSFORM_3D
	multimesh.mesh = prop_mesh
	multimesh.instance_count = transforms.size()
	multimesh.visible_instance_count = transforms.size()
	for index in range(transforms.size()):
		multimesh.set_instance_transform(index, transforms[index])

	var node := MultiMeshInstance3D.new()
	node.multimesh = multimesh
	node.material_override = materials[material_key]
	node.cast_shadow = GeometryInstance3D.SHADOW_CASTING_SETTING_OFF
	add_child(node)


func _setup_udp() -> void:
	var bind_error := udp.bind(0)
	if bind_error != OK:
		last_server_error = "UDP bind failed: %s" % bind_error
		return
	var dest_error := udp.set_dest_address(server_host, server_port)
	if dest_error != OK:
		last_server_error = "UDP destination failed: %s" % dest_error


func _update_player(delta: float) -> void:
	if local_player_entity != -1 and local_player_health <= 0.0:
		return

	var turn := 0.0
	if Input.is_key_pressed(KEY_LEFT) or Input.is_key_pressed(KEY_Q):
		turn += 1.0
	if Input.is_key_pressed(KEY_RIGHT) or Input.is_key_pressed(KEY_E):
		turn -= 1.0
	player_yaw += turn * turn_speed * delta

	var forward := Vector3(sin(player_yaw), 0.0, cos(player_yaw))
	var right := Vector3(forward.z, 0.0, -forward.x)
	var movement := Vector3.ZERO
	if Input.is_key_pressed(KEY_UP) or Input.is_key_pressed(KEY_W):
		movement += forward
	if Input.is_key_pressed(KEY_DOWN) or Input.is_key_pressed(KEY_S):
		movement -= forward
	if Input.is_key_pressed(KEY_A):
		movement -= right
	if Input.is_key_pressed(KEY_D):
		movement += right
	if movement.length_squared() > 0.0:
		player_position += movement.normalized() * move_speed * delta
		player_position.y = 0.5


func _poll_network() -> void:
	var packets_processed := 0
	while udp.get_available_packet_count() > 0 and packets_processed < max_packets_per_frame:
		packets_processed += 1
		var decoded := GridwakeProtocol.decode_server_packet(udp.get_packet())
		match decoded.get("type", ""):
			"snapshot":
				_apply_snapshot(decoded)
			"snapshot_fragment":
				_apply_snapshot_fragment(decoded)
			"metrics":
				pass
			"invalid":
				last_server_error = decoded.get("reason", "invalid packet")
	packets_backlogged = udp.get_available_packet_count()
	udp_packets_last_frame = packets_processed
	udp_packets_received += packets_processed


func _apply_snapshot(snapshot: Dictionary) -> void:
	var sequence := int(snapshot["sequence"])
	packets_received += 1
	snapshot_sequence = max(snapshot_sequence, sequence)
	udp.put_packet(GridwakeProtocol.encode_ack(sequence))
	_apply_snapshot_ops(sequence, snapshot["ops"])


func _apply_snapshot_ops(sequence: int, ops: Array) -> void:
	snapshot_sequence = max(snapshot_sequence, sequence)
	for op in ops:
		ops_received += 1
		var entity := int(op["entity"])
		if op["op"] == GridwakeProtocol.OP_DESPAWN_OR_EXIT:
			_remove_entity(entity)
			continue

		var payload := GridwakeProtocol.decode_demo_payload(op["payload"])
		if payload.is_empty():
			continue
		_upsert_entity(entity, payload)


func _apply_snapshot_fragment(fragment: Dictionary) -> void:
	fragments_received += 1
	var sequence := int(fragment["sequence"])
	var fragment_index := int(fragment["fragment_index"])
	var fragment_count := int(fragment["fragment_count"])
	if fragment_count <= 1:
		packets_received += 1
		snapshot_sequence = max(snapshot_sequence, sequence)
		udp.put_packet(GridwakeProtocol.encode_ack(sequence))
		_apply_snapshot_ops(sequence, fragment["ops"])
		return

	var key := str(sequence)
	if not snapshot_fragments.has(key):
		_track_fragment_sequence(key)
		snapshot_fragments[key] = {
			"count": fragment_count,
			"baseline": fragment["baseline"],
			"parts": {},
			"received": 0,
		}
	var state: Dictionary = snapshot_fragments[key]
	if int(state["count"]) != fragment_count:
		state = {
			"count": fragment_count,
			"baseline": fragment["baseline"],
			"parts": {},
			"received": 0,
		}
		snapshot_fragments[key] = state

	var parts: Dictionary = state["parts"]
	if not parts.has(fragment_index):
		parts[fragment_index] = fragment["ops"]
		state["received"] = int(state["received"]) + 1
		_apply_snapshot_ops(sequence, fragment["ops"])
		snapshot_fragments[key] = state

	if int(state["received"]) < fragment_count:
		return

	fragments_completed += 1
	snapshot_fragments.erase(key)
	_forget_fragment_sequence(key)
	packets_received += 1
	snapshot_sequence = max(snapshot_sequence, sequence)
	udp.put_packet(GridwakeProtocol.encode_ack(sequence))


func _track_fragment_sequence(key: String) -> void:
	if fragment_sequence_order.has(key):
		return
	fragment_sequence_order.append(key)
	var capacity: int = max(max_pending_fragment_sequences, 1)
	while fragment_sequence_order.size() > capacity:
		var old_key := String(fragment_sequence_order.pop_front())
		snapshot_fragments.erase(old_key)


func _forget_fragment_sequence(key: String) -> void:
	fragment_sequence_order.erase(key)


func _upsert_entity(entity: int, payload: Dictionary) -> void:
	_update_match_state(entity, payload)
	if int(payload["kind"]) == GridwakeProtocol.KIND_PLAYER:
		_upsert_player_node(entity, payload)
	else:
		_upsert_instanced_entity(entity, payload)


func _upsert_player_node(entity: int, payload: Dictionary) -> void:
	if instanced_entity_slots.has(entity):
		_release_instanced_entity(entity)

	var node: MeshInstance3D
	if entity_nodes.has(entity):
		node = entity_nodes[entity]
	elif _visible_entity_count() < max_visual_entities:
		node = MeshInstance3D.new()
		node.mesh = player_mesh
		entity_nodes[entity] = node
		add_child(node)
	else:
		return

	var server_position: Vector3 = payload["position"]
	var server_yaw := float(payload["yaw"])
	var radius := float(payload["radius"])
	var lod := int(payload["lod"])
	var health := _combat_health(payload)
	if local_player_entity == -1:
		local_player_entity = entity
	var is_local := local_player_entity == entity
	if is_local:
		_sync_local_player_from_server(server_position, int(payload["team"]), health)
		node.position = player_position
		node.rotation.y = player_yaw if health > 0.0 else server_yaw
	else:
		node.position = server_position
		node.rotation.y = server_yaw
	var height_scale: float = 0.35 if health <= 0.0 else 1.0
	node.scale = Vector3(radius * _lod_scale(lod), radius * _lod_scale(lod) * height_scale, radius * _lod_scale(lod))
	_apply_material(entity, node, payload)


func _sync_local_player_from_server(server_position: Vector3, team: int, health: float) -> void:
	var was_waiting_for_server := not has_local_server_position
	var was_dead := local_player_health <= 0.0
	var just_respawned := was_dead and health > 0.0
	var drift := player_position.distance_to(server_position)
	local_server_position = server_position
	has_local_server_position = true
	local_player_team = team
	if was_waiting_for_server or health <= 0.0 or just_respawned or drift > server_position_snap_distance:
		player_position = server_position
	local_player_health = health


func _upsert_instanced_entity(entity: int, payload: Dictionary) -> void:
	if entity_nodes.has(entity):
		var node: Node = entity_nodes[entity]
		entity_nodes.erase(entity)
		entity_material_keys.erase(entity)
		node.queue_free()

	if not instanced_entity_slots.has(entity) and _visible_entity_count() >= max_visual_entities:
		return

	var bucket_key := _bucket_key_for_payload(payload)
	var transform := _transform_for_payload(payload)
	var slot: Dictionary = instanced_entity_slots.get(entity, {})
	if not slot.is_empty() and slot["bucket"] != bucket_key:
		_release_instanced_entity(entity)
		slot = {}

	if slot.is_empty():
		_add_instanced_entity(entity, bucket_key, transform)
	else:
		_update_instanced_entity(entity, transform)

	if int(payload["kind"]) == GridwakeProtocol.KIND_COVER:
		_update_cover_counters(entity, payload)
	elif int(payload["kind"]) == GridwakeProtocol.KIND_IMPACT:
		impact_nodes[entity] = true
	elif int(payload["kind"]) == GridwakeProtocol.KIND_SCOREBOARD:
		score_nodes[entity] = true


func _transform_for_payload(payload: Dictionary) -> Transform3D:
	if int(payload["kind"]) == GridwakeProtocol.KIND_COVER:
		return _cover_transform_for_payload(payload)
	if int(payload["kind"]) == GridwakeProtocol.KIND_IMPACT:
		return _impact_transform_for_payload(payload)
	if int(payload["kind"]) == GridwakeProtocol.KIND_SCOREBOARD:
		return _scoreboard_transform_for_payload(payload)

	var position: Vector3 = payload["position"]
	var radius := float(payload["radius"])
	var lod := int(payload["lod"])
	var health := _combat_health(payload)
	var height_scale: float = 0.3 if health <= 0.0 else max(0.45, health)
	var scale := Vector3(radius * _lod_scale(lod), radius * _lod_scale(lod) * height_scale, radius * _lod_scale(lod))
	var basis := Basis(Vector3.UP, float(payload["yaw"]))
	basis = basis.scaled(scale)
	return Transform3D(basis, position)


func _cover_transform_for_payload(payload: Dictionary) -> Transform3D:
	var health := _cover_health(payload)
	var radius := float(payload["radius"])
	var height := lerpf(0.22, 3.4, max(health, 0.08))
	var position: Vector3 = payload["position"]
	var basis := Basis.IDENTITY.scaled(Vector3(radius * 1.8, height, radius * 1.8))
	return Transform3D(basis, Vector3(position.x, height * 0.5 - 0.06, position.z))


func _impact_transform_for_payload(payload: Dictionary) -> Transform3D:
	var position: Vector3 = payload["position"]
	var radius := float(payload["radius"])
	var phase := clampf(float(payload["phase"]), 0.0, 1.0)
	var horizontal_pulse := clampf(radius * (0.08 + phase * 0.16), 0.28, 2.6)
	var vertical_pulse := clampf(radius * (0.035 + phase * 0.055), 0.14, 0.75)
	var basis := Basis.IDENTITY.scaled(Vector3(horizontal_pulse, vertical_pulse, horizontal_pulse))
	return Transform3D(basis, Vector3(position.x, 0.12 + vertical_pulse * 0.5, position.z))


func _scoreboard_transform_for_payload(payload: Dictionary) -> Transform3D:
	var position: Vector3 = payload["position"]
	var score: float = float(payload["phase"])
	var height: float = 5.5 + min(score * 0.18, 9.0)
	var basis := Basis.IDENTITY.scaled(Vector3(3.2, height, 3.2))
	return Transform3D(basis, Vector3(position.x, height * 0.5, position.z))


func _update_match_state(entity: int, payload: Dictionary) -> void:
	var kind := int(payload["kind"])
	if (kind == GridwakeProtocol.KIND_PLAYER or kind == GridwakeProtocol.KIND_SCOREBOARD) and int(payload["lod"]) == GridwakeProtocol.LOD_FULL:
		var packed_scores := int(payload["style"])
		red_score = packed_scores & 0x0fff
		blue_score = (packed_scores >> 12) & 0x0fff
		var winner_code := (packed_scores >> 24) & 0x03
		if winner_code == 1:
			winner_team = GridwakeProtocol.TEAM_RED
		elif winner_code == 2:
			winner_team = GridwakeProtocol.TEAM_BLUE
		else:
			winner_team = -1
		round_index = ((packed_scores >> 26) & 0x3f) + 1


func _update_cover_counters(entity: int, payload: Dictionary) -> void:
	var health := _cover_health(payload)
	cover_nodes[entity] = true
	if health < 0.7:
		damaged_cover_nodes[entity] = true
	else:
		damaged_cover_nodes.erase(entity)
	if health <= 0.05:
		ruined_cover_nodes[entity] = true
	else:
		ruined_cover_nodes.erase(entity)


func _remove_entity(entity: int) -> void:
	if entity_nodes.has(entity):
		var node: Node = entity_nodes[entity]
		entity_nodes.erase(entity)
		entity_material_keys.erase(entity)
		node.queue_free()
	if instanced_entity_slots.has(entity):
		_release_instanced_entity(entity)
	cover_nodes.erase(entity)
	damaged_cover_nodes.erase(entity)
	ruined_cover_nodes.erase(entity)
	impact_nodes.erase(entity)
	score_nodes.erase(entity)


func _add_instanced_entity(entity: int, bucket_key: String, transform: Transform3D) -> void:
	var bucket := _bucket_for_key(bucket_key)
	var entities: Array = bucket["entities"]
	var transforms: Array = bucket["transforms"]
	var index := entities.size()
	_ensure_bucket_capacity(bucket, index + 1)
	entities.append(entity)
	transforms.append(transform)
	var multimesh: MultiMesh = bucket["multimesh"]
	multimesh.set_instance_transform(index, transform)
	multimesh.visible_instance_count = entities.size()
	instanced_entity_slots[entity] = {
		"bucket": bucket_key,
		"index": index,
	}


func _update_instanced_entity(entity: int, transform: Transform3D) -> void:
	var slot: Dictionary = instanced_entity_slots[entity]
	var bucket: Dictionary = instanced_buckets[slot["bucket"]]
	var index := int(slot["index"])
	var transforms: Array = bucket["transforms"]
	transforms[index] = transform
	var multimesh: MultiMesh = bucket["multimesh"]
	multimesh.set_instance_transform(index, transform)


func _release_instanced_entity(entity: int) -> void:
	var slot: Dictionary = instanced_entity_slots[entity]
	var bucket_key := String(slot["bucket"])
	var index := int(slot["index"])
	var bucket: Dictionary = instanced_buckets[bucket_key]
	var entities: Array = bucket["entities"]
	var transforms: Array = bucket["transforms"]
	var last_index := entities.size() - 1
	var multimesh: MultiMesh = bucket["multimesh"]
	if index != last_index:
		var moved_entity := int(entities[last_index])
		var moved_transform: Transform3D = transforms[last_index]
		entities[index] = moved_entity
		transforms[index] = moved_transform
		var moved_slot: Dictionary = instanced_entity_slots[moved_entity]
		moved_slot["index"] = index
		instanced_entity_slots[moved_entity] = moved_slot
		multimesh.set_instance_transform(index, moved_transform)
	entities.pop_back()
	transforms.pop_back()
	instanced_entity_slots.erase(entity)
	multimesh.visible_instance_count = entities.size()


func _bucket_for_key(bucket_key: String) -> Dictionary:
	if instanced_buckets.has(bucket_key):
		return instanced_buckets[bucket_key]

	var parts := bucket_key.split("|")
	var mesh_key := String(parts[0])
	var material_key := String(parts[1])
	var multimesh := MultiMesh.new()
	multimesh.transform_format = MultiMesh.TRANSFORM_3D
	multimesh.mesh = _mesh_for_key(mesh_key)
	multimesh.instance_count = 0
	multimesh.visible_instance_count = 0

	var node := MultiMeshInstance3D.new()
	node.multimesh = multimesh
	node.material_override = materials[material_key]
	node.cast_shadow = GeometryInstance3D.SHADOW_CASTING_SETTING_OFF
	add_child(node)

	var bucket := {
		"node": node,
		"multimesh": multimesh,
		"entities": [],
		"transforms": [],
		"capacity": 0,
	}
	instanced_buckets[bucket_key] = bucket
	return bucket


func _ensure_bucket_capacity(bucket: Dictionary, needed: int) -> void:
	var capacity := int(bucket["capacity"])
	if needed <= capacity:
		return
	var next_capacity: int = max(64, capacity * 2)
	while next_capacity < needed:
		next_capacity *= 2
	bucket["capacity"] = next_capacity
	var multimesh: MultiMesh = bucket["multimesh"]
	multimesh.instance_count = next_capacity
	var transforms: Array = bucket["transforms"]
	for index in range(transforms.size()):
		multimesh.set_instance_transform(index, transforms[index])
	multimesh.visible_instance_count = transforms.size()


func _bucket_key_for_payload(payload: Dictionary) -> String:
	return "%s|%s" % [_mesh_key_for_payload(payload), _material_key_for_payload(payload)]


func _mesh_key_for_payload(payload: Dictionary) -> String:
	match int(payload["kind"]):
		GridwakeProtocol.KIND_EFFECT:
			return "effect"
		GridwakeProtocol.KIND_COVER:
			return "cover"
		GridwakeProtocol.KIND_IMPACT:
			return "impact"
		GridwakeProtocol.KIND_SCOREBOARD:
			return "scoreboard"
		_:
			return "bot"


func _mesh_for_key(mesh_key: String) -> Mesh:
	match mesh_key:
		"effect":
			return effect_mesh
		"cover":
			return cover_mesh
		"impact":
			return impact_mesh
		"scoreboard":
			return cover_mesh
		_:
			return bot_mesh


func _visible_entity_count() -> int:
	return entity_nodes.size() + instanced_entity_slots.size()


func _apply_material(entity: int, node: MeshInstance3D, payload: Dictionary) -> void:
	var material_key := _material_key_for_payload(payload)
	if entity_material_keys.get(entity, "") == material_key:
		return
	entity_material_keys[entity] = material_key
	node.material_override = materials[material_key]


func _material_key_for_payload(payload: Dictionary) -> String:
	if int(payload["kind"]) == GridwakeProtocol.KIND_IMPACT:
		return "impact"
	if int(payload["kind"]) == GridwakeProtocol.KIND_SCOREBOARD:
		return "score_red" if int(payload["team"]) == GridwakeProtocol.TEAM_RED else "score_blue"
	if int(payload["lod"]) == GridwakeProtocol.LOD_MINIMAL:
		return "minimal"
	match int(payload["kind"]):
		GridwakeProtocol.KIND_PLAYER:
			if _combat_health(payload) <= 0.0:
				return "player_down"
			return "player_red" if int(payload["team"]) == GridwakeProtocol.TEAM_RED else "player_blue"
		GridwakeProtocol.KIND_EFFECT:
			return "effect_red" if int(payload["team"]) == GridwakeProtocol.TEAM_RED else "effect_blue"
		GridwakeProtocol.KIND_COVER:
			var health := _cover_health(payload)
			if health <= 0.05:
				return "cover_ruined"
			if health < 0.7:
				return "cover_damaged"
			return "cover_%d" % int(payload["team"] % 4)
		_:
			var health := _combat_health(payload)
			if int(payload["team"]) == GridwakeProtocol.TEAM_RED:
				return "bot_red_hurt" if health < 0.45 else "bot_red"
			return "bot_blue_hurt" if health < 0.45 else "bot_blue"


func _lod_scale(lod: int) -> float:
	match lod:
		GridwakeProtocol.LOD_FULL:
			return 1.0
		GridwakeProtocol.LOD_REDUCED:
			return 0.85
		_:
			return 0.65


func _cover_health(payload: Dictionary) -> float:
	return clampf(float(payload["yaw"]), 0.0, 1.0)


func _combat_health(payload: Dictionary) -> float:
	if int(payload["lod"]) != GridwakeProtocol.LOD_FULL:
		return 1.0
	return clampf(float(payload["phase"]), 0.0, 1.0)


func _send_input() -> void:
	udp.put_packet(GridwakeProtocol.encode_input(player_position, player_yaw, _fire_pressed()))


func _fire_pressed() -> bool:
	return Input.is_key_pressed(KEY_SPACE) or Input.is_mouse_button_pressed(MOUSE_BUTTON_LEFT)


func _update_perf(delta: float) -> void:
	perf_sample_accumulator += delta
	perf_log_accumulator += delta
	var sample_interval: float = 1.0 / max(perf_sample_hz, 0.1)
	if perf_sample_accumulator >= sample_interval:
		perf_sample_accumulator = 0.0
		_sample_perf()
	var log_interval: float = 1.0 / max(perf_log_hz, 0.1)
	if perf_log_enabled and perf_log_accumulator >= log_interval:
		perf_log_accumulator = 0.0
		_log_perf()


func _sample_perf() -> void:
	var now_msec: int = Time.get_ticks_msec()
	var elapsed_seconds: float = max(float(now_msec - perf_last_sample_msec) / 1000.0, 0.001)
	perf_fps = float(Performance.get_monitor(Performance.TIME_FPS))
	if perf_fps > 0.0:
		perf_frame_ms = 1000.0 / perf_fps
	else:
		perf_frame_ms = 0.0
	perf_process_ms = float(Performance.get_monitor(Performance.TIME_PROCESS)) * 1000.0
	perf_physics_ms = float(Performance.get_monitor(Performance.TIME_PHYSICS_PROCESS)) * 1000.0
	perf_draw_calls = int(Performance.get_monitor(Performance.RENDER_TOTAL_DRAW_CALLS_IN_FRAME))
	perf_render_objects = int(Performance.get_monitor(Performance.RENDER_TOTAL_OBJECTS_IN_FRAME))
	perf_render_primitives = int(Performance.get_monitor(Performance.RENDER_TOTAL_PRIMITIVES_IN_FRAME))
	perf_node_count = int(Performance.get_monitor(Performance.OBJECT_NODE_COUNT))
	perf_resource_count = int(Performance.get_monitor(Performance.OBJECT_RESOURCE_COUNT))
	perf_static_memory_mb = float(Performance.get_monitor(Performance.MEMORY_STATIC)) / (1024.0 * 1024.0)
	perf_udp_packets_per_sec = float(udp_packets_received - perf_last_udp_packets) / elapsed_seconds
	perf_snapshots_per_sec = float(packets_received - perf_last_snapshots) / elapsed_seconds
	perf_ops_per_sec = float(ops_received - perf_last_ops) / elapsed_seconds
	perf_fragments_per_sec = float(fragments_received - perf_last_fragments) / elapsed_seconds
	perf_last_sample_msec = now_msec
	perf_last_udp_packets = udp_packets_received
	perf_last_snapshots = packets_received
	perf_last_ops = ops_received
	perf_last_fragments = fragments_received


func _log_perf() -> void:
	print(
		"perf fps=%.1f frame_ms=%.2f process_ms=%.2f physics_ms=%.2f visible=%d instanced=%d buckets=%d nodes=%d draw_calls=%d render_objects=%d primitives=%d udp_pps=%.1f snapshots_ps=%.1f ops_ps=%.1f fragments_ps=%.1f backlog=%d pending_fragments=%d memory_mb=%.1f" % [
			perf_fps,
			perf_frame_ms,
			perf_process_ms,
			perf_physics_ms,
			_visible_entity_count(),
			instanced_entity_slots.size(),
			instanced_buckets.size(),
			perf_node_count,
			perf_draw_calls,
			perf_render_objects,
			perf_render_primitives,
			perf_udp_packets_per_sec,
			perf_snapshots_per_sec,
			perf_ops_per_sec,
			perf_fragments_per_sec,
			packets_backlogged,
			snapshot_fragments.size(),
			perf_static_memory_mb,
		]
	)


func _update_camera(delta: float) -> void:
	var forward := Vector3(sin(player_yaw), 0.0, cos(player_yaw))
	var desired_position := player_position - forward * camera_follow_distance + Vector3(0.0, camera_height, 0.0)
	var desired_look := player_position + forward * 16.0 + Vector3(0.0, 1.1, 0.0)
	var alpha := 1.0 - exp(-camera_smoothing * delta)
	if not camera_initialized:
		camera.position = desired_position
		camera_look_position = desired_look
		camera_initialized = true
	else:
		camera.position = camera.position.lerp(desired_position, alpha)
		camera_look_position = camera_look_position.lerp(desired_look, alpha)
	camera.look_at(camera_look_position, Vector3.UP)


func _update_hud() -> void:
	var status := "ALIVE" if local_player_health > 0.0 else "RESPAWNING"
	var winner_text := ""
	if winner_team != -1:
		status = "ROUND END"
		winner_text = "  WINNER %s" % _team_name(winner_team)
	var text := "Gridwake Deathmatch\nROUND %d  RED %d  BLUE %d%s\nteam %s  health %d%%  %s" % [
		round_index,
		red_score,
		blue_score,
		winner_text,
		_team_name(local_player_team),
		int(round(local_player_health * 100.0)),
		status,
	]
	if show_perf_overlay:
		text += "\nserver %s:%d\nfps %.1f frame %.2fms proc %.2fms phys %.2fms\nvisible %d / cap %d instanced %d buckets %d nodes %d resources %d\nrender draw %d objects %d prim %d mem %.1fMB\ncover %d damaged %d ruined %d impacts %d score_nodes %d\nsnapshot %d udp %d last %d %.1f/s backlog %d fragments %d pending %d %.1f/s ops %d %.1f/s" % [
			server_host,
			server_port,
			perf_fps,
			perf_frame_ms,
			perf_process_ms,
			perf_physics_ms,
			_visible_entity_count(),
			max_visual_entities,
			instanced_entity_slots.size(),
			instanced_buckets.size(),
			perf_node_count,
			perf_resource_count,
			perf_draw_calls,
			perf_render_objects,
			perf_render_primitives,
			perf_static_memory_mb,
			cover_nodes.size(),
			damaged_cover_nodes.size(),
			ruined_cover_nodes.size(),
			impact_nodes.size(),
			score_nodes.size(),
			snapshot_sequence,
			udp_packets_received,
			udp_packets_last_frame,
			perf_udp_packets_per_sec,
			packets_backlogged,
			fragments_received,
			snapshot_fragments.size(),
			perf_fragments_per_sec,
			ops_received,
			perf_ops_per_sec,
		]
	if last_server_error != "":
		text += "\n%s" % last_server_error
	hud.text = text
	if crosshair_root != null:
		crosshair_root.visible = local_player_health > 0.0 and Input.get_mouse_mode() == Input.MOUSE_MODE_CAPTURED


func _team_name(team: int) -> String:
	match team:
		GridwakeProtocol.TEAM_RED:
			return "RED"
		GridwakeProtocol.TEAM_BLUE:
			return "BLUE"
		_:
			return "JOINING"
