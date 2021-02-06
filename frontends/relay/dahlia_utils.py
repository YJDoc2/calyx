import subprocess
import os

from typing import List
from tempfile import NamedTemporaryFile, TemporaryFile

from futil.ast import *
from futil.utils import block
from relay_utils import DahliaFuncDef, get_dims

# Starting index variable name
# for Dahlia array iteration.
CHARACTER_I = chr(ord('i'))


def next_character(ch: chr, dir: int = 1) -> chr:
    """Returns the next character after 'ch'.
    If `dir` is positive, then will return 'ch' + 1. Otherwise, it will return 'ch' - 1.
    E.g. next_character('a') == 'b'
    """
    return chr(ord(ch) + 1) if dir > 0 else chr(ord(ch) - 1)


def emit_dahlia_params(fd: DahliaFuncDef) -> str:
    """Emits a comma-separated string of Dahlia
    memory declarations, e.g.
    `X: ubit<32> [1][10], a: ufix<8, 2>[3]`
    """
    cells = []
    for cell in fd.args + [fd.dest]:
        cell_str = f'{cell.id.name}: {fd.data_type}'

        dims = get_dims(cell.comp)
        args = cell.comp.args
        for i in range(0, dims):
            cell_str += f'[{args[i + 1]}]'

        cells.append(cell_str)

    return ', '.join(cells)


def emit_dahlia_definition(fd: DahliaFuncDef, body: str) -> str:
    """Emits a Dahlia definition, e.g.
    `def foo(a: ubit<32>) = { ... }`
    """
    params = emit_dahlia_params(fd)
    return block(
        f'def {fd.function_id}({params}) =',
        body,
        sep=''
    )


def emit_dahlia_loop(fd: DahliaFuncDef, body: str, num_dims: int) -> str:
    """Emits a Dahlia loop over `num_dims` with `body`
    nested inside. Many tensor functions share the
    same control flow:
    (1) Iterate `num_dims` times, and
    (2) do some work in the body.

    For example, if body == `X`, then this
    will return:
    ```
    for (let i: ubit<X> = 0..M) {
      for (let j: ubit<Y> = 0..N) {
        X;
      }
    }
    ```
    """
    var_name = CHARACTER_I
    args = fd.dest.comp.args

    # Generate loop headers.
    headers = []
    for i in range(num_dims):
        size = args[i + 1]
        idx_size = args[i + 1 + num_dims]
        headers.append(
            f'for (let {var_name}: ubit<{idx_size}> = 0..{size})'
        )
        var_name = next_character(var_name)

    headers.reverse()

    # Generate loop blocks.
    for i in range(num_dims):
        b = body if i == 0 else headers[i - 1]
        headers[i] = block(headers[i], b, sep='')
    return headers[-1]


def dahlia_to_futil(dahlia_definitions: str) -> str:
    """Takes in a string representation of a Dahlia
    function definitions, and lowers it to FuTIL.
    This does not include the `import` statements,
    nor the empty `main` component.
    """
    with NamedTemporaryFile() as tf0, NamedTemporaryFile() as tf1:
        tf0.write(bytes(dahlia_definitions, 'UTF-8'))
        tf0.seek(0), tf1.seek(0)
        command = f"""fud e --from dahlia {tf0.name} --to futil > {tf1.name} -q"""
        subprocess.Popen(command, stdout=subprocess.PIPE, shell=True).communicate()

        components = tf1.read().decode()
        # Don't double-import the primitives library.
        begin = components.find('component')
        # Don't import the empty main component.
        end = components.find('component main() -> () {')
        return components[begin:end]
