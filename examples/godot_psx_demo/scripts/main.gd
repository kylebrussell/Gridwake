extends Node3D

const GridwakeProtocol := preload("res://scripts/gridwake_protocol.gd")

@export var server_host := "127.0.0.1"
@export var server_port := 3456
@export var input_send_hz := 30.0
@export var move_speed := 28.0
@export var turn_speed := 2.4
@export var max_visual_entities := 6000

var udp := PacketPeerUDP.new()
var entity_nodes: Dictionary = {}
var player_position := Vector3(0.0, 0.5, 18.0)
var player_yaw := 0.0
var input_accumulator := 0.0
var snapshot_sequence := -1
var packets_received := 0
var ops_received := 0
var last_server_error := ""

var bot_mesh: BoxMesh
var player_mesh: BoxMesh
var effect_mesh: SphereMesh
var materials: Dictionary = {}
var camera: Camera3D
var hud: Label

func _ready() -> void:
	_setup_rendering()
	_setup_world()
	_setup_udp()
	_send_input()


func _exit_tree() -> void:
	udp.close()


func _physics_process(delta: float) -> void:
	_update_player(delta)
	_poll_network()
	input_accumulator += delta
	if input_accumulator >= 1.0 / input_send_hz:
		input_accumulator = 0.0
		_send_input()
	_update_camera()
	_update_hud()


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

	_create_materials()
	_create_floor()

	var canvas := CanvasLayer.new()
	hud = Label.new()
	hud.position = Vector2(16.0, 16.0)
	hud.add_theme_color_override("font_color", Color(0.78, 0.95, 1.0))
	hud.add_theme_color_override("font_shadow_color", Color.BLACK)
	hud.add_theme_constant_override("shadow_offset_x", 2)
	hud.add_theme_constant_override("shadow_offset_y", 2)
	canvas.add_child(hud)
	add_child(canvas)


func _create_materials() -> void:
	materials["bot_0"] = _material(Color(0.95, 0.25, 0.32))
	materials["bot_1"] = _material(Color(0.23, 0.62, 1.0))
	materials["bot_2"] = _material(Color(0.95, 0.82, 0.22))
	materials["player"] = _material(Color(0.75, 1.0, 0.72))
	materials["effect"] = _material(Color(0.98, 0.42, 0.92))
	materials["minimal"] = _material(Color(0.52, 0.57, 0.66))
	materials["floor"] = _material(Color(0.13, 0.16, 0.18))
	materials["grid"] = _material(Color(0.22, 0.28, 0.32))


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


func _setup_udp() -> void:
	var bind_error := udp.bind(0)
	if bind_error != OK:
		last_server_error = "UDP bind failed: %s" % bind_error
		return
	var dest_error := udp.set_dest_address(server_host, server_port)
	if dest_error != OK:
		last_server_error = "UDP destination failed: %s" % dest_error


func _update_player(delta: float) -> void:
	var turn := 0.0
	if Input.is_key_pressed(KEY_LEFT) or Input.is_key_pressed(KEY_A):
		turn += 1.0
	if Input.is_key_pressed(KEY_RIGHT) or Input.is_key_pressed(KEY_D):
		turn -= 1.0
	player_yaw += turn * turn_speed * delta

	var forward := Vector3(sin(player_yaw), 0.0, cos(player_yaw))
	var right := Vector3(forward.z, 0.0, -forward.x)
	var movement := Vector3.ZERO
	if Input.is_key_pressed(KEY_UP) or Input.is_key_pressed(KEY_W):
		movement += forward
	if Input.is_key_pressed(KEY_DOWN) or Input.is_key_pressed(KEY_S):
		movement -= forward
	if Input.is_key_pressed(KEY_Q):
		movement -= right
	if Input.is_key_pressed(KEY_E):
		movement += right
	if movement.length_squared() > 0.0:
		player_position += movement.normalized() * move_speed * delta
		player_position.y = 0.5


func _poll_network() -> void:
	while udp.get_available_packet_count() > 0:
		var decoded := GridwakeProtocol.decode_server_packet(udp.get_packet())
		match decoded.get("type", ""):
			"snapshot":
				_apply_snapshot(decoded)
			"metrics":
				pass
			"invalid":
				last_server_error = decoded.get("reason", "invalid packet")


func _apply_snapshot(snapshot: Dictionary) -> void:
	packets_received += 1
	snapshot_sequence = snapshot["sequence"]
	udp.put_packet(GridwakeProtocol.encode_ack(snapshot_sequence))

	for op in snapshot["ops"]:
		ops_received += 1
		var entity := int(op["entity"])
		if op["op"] == GridwakeProtocol.OP_DESPAWN_OR_EXIT:
			_remove_entity(entity)
			continue

		var payload := GridwakeProtocol.decode_demo_payload(op["payload"])
		if payload.is_empty():
			continue
		_upsert_entity(entity, payload)


func _upsert_entity(entity: int, payload: Dictionary) -> void:
	var node: MeshInstance3D
	if entity_nodes.has(entity):
		node = entity_nodes[entity]
	elif entity_nodes.size() < max_visual_entities:
		node = _create_entity_node(payload)
		entity_nodes[entity] = node
		add_child(node)
	else:
		return

	node.position = payload["position"]
	node.rotation.y = payload["yaw"]
	var radius := float(payload["radius"])
	var lod := int(payload["lod"])
	node.scale = Vector3.ONE * (radius * _lod_scale(lod))
	node.material_override = _material_for_payload(payload)


func _create_entity_node(payload: Dictionary) -> MeshInstance3D:
	var node := MeshInstance3D.new()
	match int(payload["kind"]):
		GridwakeProtocol.KIND_PLAYER:
			node.mesh = player_mesh
		GridwakeProtocol.KIND_EFFECT:
			node.mesh = effect_mesh
		_:
			node.mesh = bot_mesh
	return node


func _remove_entity(entity: int) -> void:
	if not entity_nodes.has(entity):
		return
	var node: Node = entity_nodes[entity]
	entity_nodes.erase(entity)
	node.queue_free()


func _material_for_payload(payload: Dictionary) -> StandardMaterial3D:
	if int(payload["lod"]) == GridwakeProtocol.LOD_MINIMAL:
		return materials["minimal"]
	match int(payload["kind"]):
		GridwakeProtocol.KIND_PLAYER:
			return materials["player"]
		GridwakeProtocol.KIND_EFFECT:
			return materials["effect"]
		_:
			return materials["bot_%d" % int(payload["team"] % 3)]


func _lod_scale(lod: int) -> float:
	match lod:
		GridwakeProtocol.LOD_FULL:
			return 1.0
		GridwakeProtocol.LOD_REDUCED:
			return 0.85
		_:
			return 0.65


func _send_input() -> void:
	udp.put_packet(GridwakeProtocol.encode_input(player_position, player_yaw))


func _update_camera() -> void:
	var forward := Vector3(sin(player_yaw), 0.0, cos(player_yaw))
	camera.position = player_position - forward * 18.0 + Vector3(0.0, 13.0, 0.0)
	camera.look_at(player_position + forward * 7.0, Vector3.UP)


func _update_hud() -> void:
	hud.text = "Gridwake Godot Demo\nserver %s:%d\nvisible %d / cap %d\nsnapshot %d packets %d ops %d\n%s" % [
		server_host,
		server_port,
		entity_nodes.size(),
		max_visual_entities,
		snapshot_sequence,
		packets_received,
		ops_received,
		last_server_error,
	]
