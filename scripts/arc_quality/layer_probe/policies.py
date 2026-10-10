"""Complete, explicit fixture policies; no shipping recommendation."""
CLASSES = ('attention', 'dense', 'shared', 'embedding', 'head')
NAMES = ('int16', 'legacy') + CLASSES
RUN_NAMES = ('default',) + NAMES


def policy(name):
    if name == 'legacy': return None
    if name == 'default': name = 'int16'
    if name not in NAMES: raise ValueError('unknown policy')
    return {'version': 1, **{k: 'int16' if name in ('int16', k) else 'int8' for k in CLASSES}}


def validate(value):
    if value is None: return
    if not isinstance(value, dict) or set(value) != {'version', *CLASSES}:
        raise ValueError('incomplete/unknown precision fields')
    if type(value['version']) is not int or value['version'] != 1 or any(value[k] not in ('int8', 'int16') for k in CLASSES):
        raise ValueError('invalid precision policy')
