package cz.moneta.smartbanka.common.mvi


fun interface Router<Effect> {
  fun handle(effect: Effect)
}
