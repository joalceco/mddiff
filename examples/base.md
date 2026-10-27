# Una revisión, con contexto

## Propuesta de producto

mddiff permite leer un documento Markdown y revisar una segunda versión sin salir de la lectura. El documento original permanece a la izquierda y la propuesta aparece a la derecha.

La primera versión está pensada para revisar cambios que propone una IA. La integración recibe rutas locales y abre la comparación en el navegador.

## El flujo de revisión

1. Abre el documento original.
2. Envía la ruta de la propuesta mediante la API.
3. Revisa los bloques que cambiaron.

> Una buena revisión conserva suficiente contexto para entender por qué cambia un párrafo.

## Alcance inicial

| Capacidad | Primera versión |
| --- | --- |
| Lectura Markdown | Encabezados, listas y código |
| Comparación | Dos versiones renderizadas |
| Integración | API HTTP local |
| Escritura de archivos | No incluida |

## Implementación

El núcleo se implementa en Rust. La interfaz utiliza el navegador y recibe una notificación cuando se abre otra comparación.

```rust
fn main() {
    println!("Leer primero, revisar después.");
}
```

## Cómo sabremos que funciona

La API debe abrir una comparación sin que el lector recargue la página. El programa no modifica los archivos que está leyendo.

La sintaxis del documento sigue las reglas de [CommonMark][markdown].

[markdown]: https://spec.commonmark.org/
