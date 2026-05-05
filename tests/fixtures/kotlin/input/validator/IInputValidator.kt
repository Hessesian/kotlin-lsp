package cz.moneta.smartbanka.mobile.commonui.component.input.validator


fun interface IInputValidator<in In, out Out> {
  fun validate(value: In): Out?
}
